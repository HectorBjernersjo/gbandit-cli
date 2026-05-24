use chrono::Local;

#[derive(Clone, Copy)]
pub(crate) struct Printer {
    pub(crate) verbose: bool,
    pub(crate) timestamps: bool,
    pub(crate) json: bool,
}

impl Printer {
    /// CLI-side progress line. Always shown; timestamp follows the flag.
    pub(crate) fn progress(&self, msg: impl AsRef<str>) {
        let line = if self.timestamps {
            format!("{} {}", local_timestamp(), msg.as_ref())
        } else {
            msg.as_ref().to_string()
        };
        if self.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// Info-level Log event (headline): always shown.
    pub(crate) fn info(&self, stage: &str, created_at: Option<&str>, text: &str) {
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, self.verbose);
        }
    }

    /// Debug-level Log event: shown only with `--verbose`.
    pub(crate) fn debug(&self, stage: &str, created_at: Option<&str>, text: &str) {
        if !self.verbose {
            return;
        }
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, true);
        }
    }

    /// Status transition. Hidden in default mode; failed stages are surfaced
    /// separately with their buffered log.
    pub(crate) fn status(
        &self,
        stage: &str,
        created_at: Option<&str>,
        kind: &str,
        detail: Option<&str>,
    ) {
        if !self.verbose {
            return;
        }
        let line = match detail {
            Some(d) if !d.is_empty() => format!("{kind}: {d}"),
            _ => kind.to_string(),
        };
        self.print_event_line(stage, created_at, &line, true);
    }

    pub(crate) fn print_event_line(
        &self,
        stage: &str,
        created_at: Option<&str>,
        line: &str,
        with_label: bool,
    ) {
        let prefix_ts = if self.timestamps {
            created_at
                .and_then(format_event_timestamp)
                .unwrap_or_else(local_timestamp)
        } else {
            String::new()
        };
        let label = if with_label {
            format!("[{stage}] ")
        } else {
            String::new()
        };
        let rendered = if prefix_ts.is_empty() {
            format!("{label}{line}")
        } else {
            format!("{prefix_ts} {label}{line}")
        };
        if self.json {
            eprintln!("{rendered}");
        } else {
            println!("{rendered}");
        }
    }
}

/// `HH:MM:SS.cc` from the local wall clock.
fn local_timestamp() -> String {
    let now = Local::now();
    let cs = now.timestamp_subsec_millis() / 10;
    format!("{}.{cs:02}", now.format("%H:%M:%S"))
}

/// RFC3339 → `HH:MM:SS.cc` in local time. `None` on parse failure.
fn format_event_timestamp(created_at: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(created_at).ok()?;
    let local = parsed.with_timezone(&Local);
    let cs = local.timestamp_subsec_millis() / 10;
    Some(format!("{}.{cs:02}", local.format("%H:%M:%S")))
}
