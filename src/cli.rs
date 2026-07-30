use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "gbandit", version = crate::BUILD_VERSION)]
pub(crate) struct Cli {
    /// Show subprocess output and per-stage status events.
    #[arg(short, long, global = true)]
    pub(crate) verbose: bool,
    /// Prefix every line with a timestamp.
    #[arg(short, long, global = true)]
    pub(crate) timestamps: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Login,
    Whoami,
    /// Update the gbandit CLI from GitHub releases.
    Update {
        /// Install a specific release tag instead of the latest release.
        #[arg(long)]
        tag: Option<String>,
    },
    Sql {
        query: String,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Deploy {
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
        /// Becomes the git commit message and the checkpoint label.
        #[arg(short, long)]
        message: Option<String>,
        /// Overwrite the latest deployed code lineage when this checkout does
        /// not contain it. Use after intentional history rewrites.
        #[arg(long)]
        overwrite: bool,
        /// Agent warm build: deploys the fresh scaffold to warm the build
        /// cache without claiming deploy lineage. Server-side it is skipped
        /// entirely if the project already has a succeeded deploy.
        #[arg(long, hide = true)]
        baseline: bool,
        /// Create the project on the platform without asking when it does
        /// not exist yet.
        #[arg(long)]
        create: bool,
        /// Return after creating the Pipeline Run instead of waiting for completion.
        #[arg(long)]
        detach: bool,
        /// Emit stable machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    Logs {
        component: LogTarget,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Scaffold the game template into ./<name>/ (or the current directory
    /// when NAME is "."). Purely local — the platform project is created on
    /// your first `gbandit deploy`.
    Scaffold {
        /// Project slug + directory name. Use "." for in-place scaffolding.
        /// Prompted if omitted.
        name: Option<String>,
        /// Display title, stored as `title` in gbandit.json — deploy keeps
        /// the platform title in sync with that field. Prompted if omitted.
        #[arg(long)]
        title: Option<String>,
        /// Scaffold into this directory instead of ./<name>/. Used by the
        /// Pi Agent entrypoint.
        #[arg(long, hide = true)]
        target: Option<String>,
    },
    /// Print platform documentation as raw markdown, fetched from
    /// docs.gbandit.com so it always reflects the current deploy contract.
    Docs {
        /// Page to print (e.g. "deploy", "auth", "heartbeat"). Omit to
        /// print the llms.txt index of available pages.
        page: Option<String>,
        /// Print the entire documentation in one output (llms-full.txt).
        #[arg(long)]
        full: bool,
    },
    Logout,
}

#[derive(Subcommand)]
pub(crate) enum ProjectAction {
    /// Delete a project entirely. No undo. Project-member and human-only.
    Delete {
        /// Project slug to delete.
        slug: String,
        /// Skip the type-the-slug interactive confirm. Use only in scripts.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum MigrateAction {
    /// Roll the dev tenant DB to a specific migration version. Dev-only —
    /// prod is forward-only by ADR 0002.
    DownTo {
        /// Target migration version. Anything below the project's Migration
        /// Floor (max version ever applied to prod) is rejected.
        target: i64,
        #[arg(long)]
        project: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum EnvAction {
    Set {
        /// KEY=VALUE pairs to set
        #[arg(required = true)]
        pairs: Vec<String>,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    List {
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Delete {
        key: String,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
}

/// Mirrors `tenant_routing::Environment`; duplicated to avoid the dep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum Environment {
    Dev,
    Prod,
}

impl Environment {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Prod => "prod",
        }
    }
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, ValueEnum)]
pub(crate) enum LogTarget {
    Backend,
    Frontend,
}
