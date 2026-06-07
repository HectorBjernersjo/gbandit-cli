use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::http::parse_error;
use crate::printer::Printer;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
enum PipelineStageKey {
    FrontendBuild,
    FrontendPublish,
    BackendMigrate,
    BackendBuild,
    BackendDeploy,
}

impl PipelineStageKey {
    fn as_str(self) -> &'static str {
        match self {
            Self::FrontendBuild => "frontend_build",
            Self::FrontendPublish => "frontend_publish",
            Self::BackendMigrate => "backend_migrate",
            Self::BackendBuild => "backend_build",
            Self::BackendDeploy => "backend_deploy",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct PipelineChildStatus {
    /// `None` when the stage hasn't reported yet. Used on snapshot/reconnect
    /// to dump already-failed stages without waiting for a missed event.
    #[serde(default)]
    status: Option<String>,
    log_output: String,
}

#[derive(Debug, Deserialize, Clone)]
struct PipelineRun {
    /// Planned stages in execution order — drives which child statuses the
    /// CLI watches and reports.
    stages: Vec<PipelineStageKey>,
    status: String,
    error_summary: Option<String>,
    frontend_build: PipelineChildStatus,
    frontend_publish: PipelineChildStatus,
    backend_migrate: PipelineChildStatus,
    backend_build: PipelineChildStatus,
    backend_deploy: PipelineChildStatus,
}

#[derive(Debug, Deserialize)]
struct PipelineStreamDelta {
    #[serde(default)]
    created_at: Option<String>,
    event: StreamEvent,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    stage: String,
    kind: String,
    /// `info` (headline, always shown) or `debug` (verbose-only). ADR 0003.
    #[serde(default = "default_log_level")]
    level: String,
    #[serde(default)]
    detail: Option<String>,
}

fn default_log_level() -> String {
    "debug".to_string()
}

/// Per-stage Debug-line buffer, dumped if the stage fails.
type StageLogBuffer = HashMap<String, Vec<(Option<String>, String)>>;

pub(crate) async fn watch_deploy_pipeline(
    printer: &Printer,
    client: &reqwest::Client,
    platform_api_origin: &str,
    token: &str,
    pipeline_run_id: i64,
) -> Result<()> {
    let mut response = client
        .get(format!(
            "{platform_api_origin}/pipelines/{pipeline_run_id}/stream"
        ))
        .bearer_auth(token)
        .header("accept", "text/event-stream")
        .send()
        .await
        .context("failed to open deploy event stream")?;
    if !response.status().is_success() {
        bail!(
            "failed to follow deploy progress: {}",
            parse_error(response).await
        );
    }

    let mut buf = String::new();
    let mut buffers: StageLogBuffer = HashMap::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read event stream chunk")?
    {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let raw_event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(outcome) = handle_sse_event(printer, &mut buffers, &raw_event)? {
                return outcome;
            }
        }
    }
    bail!(
        "lost connection to the deploy event stream — the deploy may still be running on the platform. \
         Check the project dashboard or `gbandit logs backend` to see how it ended."
    )
}

/// `Some(result)` when the deploy reaches a terminal state.
fn handle_sse_event(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    raw: &str,
) -> Result<Option<Result<()>>> {
    let mut event_type = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event_type = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    let data = data_lines.join("\n");

    match event_type.as_str() {
        "snapshot" => {
            let snap: PipelineRun =
                serde_json::from_str(&data).context("failed to decode snapshot")?;
            // Reconnect catch-up: seed buffers from the snapshot's
            // log_output, and dump any stage already in `failed` since
            // no Failed event will arrive.
            for (label, child) in pipeline_stages(&snap) {
                if !child.log_output.is_empty() {
                    buffers.insert(label.to_string(), vec![(None, child.log_output.clone())]);
                    if printer.verbose {
                        printer.debug(label, None, &child.log_output);
                    }
                }
                if child.status.as_deref() == Some("failed") {
                    dump_failed_stage(printer, buffers, label, None);
                }
            }
            printer.status("pipeline", None, "status", Some(&snap.status));
            if is_terminal_status(&snap.status) {
                return Ok(Some(finish_terminal(
                    printer,
                    &snap.status,
                    snap.error_summary,
                )));
            }
        }
        "" | "pipeline_event" => {
            let delta: PipelineStreamDelta = match serde_json::from_str(&data) {
                Ok(d) => d,
                Err(_) => return Ok(None),
            };
            let stage = delta.event.stage.clone();
            let created_at = delta.created_at.as_deref();
            match delta.event.kind.as_str() {
                "log" => {
                    let text = delta.event.detail.as_deref().unwrap_or("");
                    let is_info = delta.event.level == "info";
                    if is_info {
                        printer.info(&stage, created_at, text);
                    } else {
                        // Buffer for potential failure dump; stream in verbose.
                        buffers
                            .entry(stage.clone())
                            .or_default()
                            .push((delta.created_at.clone(), text.to_string()));
                        printer.debug(&stage, created_at, text);
                    }
                }
                "started" | "succeeded" => {
                    if stage != "pipeline" {
                        printer.status(&stage, created_at, delta.event.kind.as_str(), None);
                    }
                }
                "skipped" => {
                    printer.status(&stage, created_at, "skipped", delta.event.detail.as_deref());
                }
                "failed" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    if stage != "pipeline" {
                        // Always surface failures, even in default mode.
                        printer.print_event_line(
                            &stage,
                            created_at,
                            &format!("failed: {reason}"),
                            true,
                        );
                        dump_failed_stage(printer, buffers, &stage, created_at);
                    }
                }
                "cancelled" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    printer.print_event_line(
                        &stage,
                        created_at,
                        &format!("cancelled: {reason}"),
                        true,
                    );
                }
                _ => {}
            }

            if stage == "pipeline" {
                match delta.event.kind.as_str() {
                    "succeeded" => {
                        printer.progress("Deploy succeeded.");
                        return Ok(Some(Ok(())));
                    }
                    "failed" => {
                        let summary = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| "deploy failed".into());
                        return Ok(Some(Err(anyhow::anyhow!("{}", summary))));
                    }
                    "cancelled" => {
                        let reason = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| "cancelled".into());
                        return Ok(Some(Err(anyhow::anyhow!("deploy cancelled: {}", reason))));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

/// Print the buffered Debug log for a failed stage, with `--- log ---`
/// markers so the failure seam stands out.
fn dump_failed_stage(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    stage: &str,
    created_at: Option<&str>,
) {
    let entries = buffers.remove(stage).unwrap_or_default();
    if entries.is_empty() && printer.verbose {
        return;
    }
    println!("--- {stage} log ---");
    for (entry_ts, text) in &entries {
        for line in text.lines() {
            printer.print_event_line(stage, entry_ts.as_deref(), line, false);
        }
    }
    println!("--- end {stage} log ---");
    let _ = created_at;
}

fn finish_terminal(printer: &Printer, status: &str, error_summary: Option<String>) -> Result<()> {
    if status == "succeeded" {
        printer.progress("Deploy succeeded.");
        Ok(())
    } else {
        bail!(
            "{}",
            error_summary.unwrap_or_else(|| format!("deploy {status}"))
        )
    }
}

fn pipeline_stages(pipeline: &PipelineRun) -> Vec<(&'static str, &PipelineChildStatus)> {
    pipeline
        .stages
        .iter()
        .map(|stage| {
            let child = match stage {
                PipelineStageKey::FrontendBuild => &pipeline.frontend_build,
                PipelineStageKey::FrontendPublish => &pipeline.frontend_publish,
                PipelineStageKey::BackendMigrate => &pipeline.backend_migrate,
                PipelineStageKey::BackendBuild => &pipeline.backend_build,
                PipelineStageKey::BackendDeploy => &pipeline.backend_deploy,
            };
            (stage.as_str(), child)
        })
        .collect()
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "skipped" | "cancelled")
}
