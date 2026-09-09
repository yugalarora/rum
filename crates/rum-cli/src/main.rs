//! rum: a fast, parallel, Rust-native yum/dnf-compatible package manager.
//!
//! This is the v0.1 skeleton. Read/query commands that do not mutate system
//! state are implemented against the real host config; state-changing commands
//! (`install`, `remove`, `upgrade`) are stubbed and will be wired to the solver
//! and the librpm transaction layer in later milestones.

mod commands;
mod glob;
mod sys;

use clap::{Parser, Subcommand};

/// rum: Rust yum/dnf-compatible package manager.
#[derive(Parser, Debug)]
#[command(
    name = "rum",
    version,
    about = "A fast, parallel, Rust-native yum/dnf-compatible package manager",
    long_about = None,
)]
struct Cli {
    /// Increase verbosity (-v, -vv). Overrides RUM_LOG.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Assume yes to all prompts (dnf -y).
    #[arg(short = 'y', long = "assumeyes", global = true)]
    assume_yes: bool,

    /// Do not prompt; assume no. Overrides -y for state-changing ops.
    #[arg(long = "assumeno", global = true)]
    assume_no: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List enabled/all software repositories (like `dnf repolist`).
    Repolist {
        /// Show all repos including disabled ones.
        #[arg(long)]
        all: bool,
        /// Show only enabled repos (default).
        #[arg(long)]
        enabled: bool,
        /// Show only disabled repos.
        #[arg(long)]
        disabled: bool,
    },

    /// List packages (installed / available). [planned]
    List {
        /// What to list: installed, available, or all.
        #[arg(default_value = "all")]
        what: String,
        /// Optional name globs to filter by.
        patterns: Vec<String>,
    },

    /// Show detailed information about a package. [planned]
    Info { packages: Vec<String> },

    /// Search package metadata by keyword. [planned]
    Search { terms: Vec<String> },

    /// Find which package provides a file or capability. [planned]
    Provides { spec: String },

    /// Check for available updates without installing. [planned]
    #[command(name = "check-update")]
    CheckUpdate { packages: Vec<String> },

    /// Download packages (optionally with dependencies) without installing.
    Download {
        packages: Vec<String>,
        /// Also download the full dependency closure.
        #[arg(long)]
        resolve: bool,
        /// Directory to write RPMs into (default: current directory).
        #[arg(long, default_value = ".")]
        destdir: String,
    },

    /// Refresh and cache repository metadata (like `dnf makecache`). [planned]
    Makecache,

    /// Install packages. [planned: solve now, commit via librpm later]
    Install { packages: Vec<String> },

    /// Remove packages. [planned]
    Remove { packages: Vec<String> },

    /// Upgrade packages (all, or the named ones). [planned]
    Upgrade { packages: Vec<String> },

    /// Clean cached data (like `dnf clean`). [planned]
    Clean {
        /// What to clean: all, metadata, packages.
        #[arg(default_value = "all")]
        what: String,
    },
}

fn main() -> anyhow::Result<()> {
    reset_sigpipe();
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // --assumeno overrides --assumeyes for state-changing operations.
    let assume_yes = cli.assume_yes && !cli.assume_no;

    match cli.command {
        Command::Repolist {
            all,
            enabled,
            disabled,
        } => commands::repolist::run(all, enabled, disabled),
        Command::List { what, patterns } => commands::list::run(&what, &patterns),
        Command::Info { packages } => commands::info::run(&packages),
        Command::Search { terms } => commands::search::run(&terms),
        Command::Makecache => commands::makecache::run(),
        Command::CheckUpdate { packages } => commands::check_update::run(&packages),
        Command::Download {
            packages,
            resolve,
            destdir,
        } => commands::download::run(&packages, resolve, std::path::Path::new(&destdir)),
        Command::Install { packages } => commands::install::run(&packages, assume_yes),
        Command::Remove { packages } => commands::remove::run(&packages, assume_yes),
        Command::Upgrade { packages } if !packages.is_empty() => {
            // `upgrade <pkgs>` is install semantics (rpm -U upgrades in place).
            commands::install::run(&packages, assume_yes)
        }
        Command::Clean { what } => commands::clean::run(&what),
        other => {
            // Every other command is a recognized dnf verb we have not wired
            // up yet. Be explicit rather than silently doing nothing.
            commands::not_yet(&other)
        }
    }
}

/// Restore the default SIGPIPE disposition on Unix.
///
/// Rust sets SIGPIPE to SIG_IGN at startup, so writing to a closed pipe (e.g.
/// `rum list installed | head`) surfaces as an EPIPE write error and panics.
/// CLI tools want the classic behaviour: be terminated by SIGPIPE and exit
/// quietly (status 141). Matches what ripgrep/fd do.
#[cfg(unix)]
fn reset_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13; // same value on Linux and macOS
    const SIG_DFL: usize = 0;
    // SAFETY: resetting a signal handler to the default is a well-defined,
    // async-signal-safe operation done once before any threads are spawned.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_env("RUM_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

// Give `commands::not_yet` a Debug handle on the command name.
impl Command {
    fn verb(&self) -> &'static str {
        match self {
            Command::Repolist { .. } => "repolist",
            Command::List { .. } => "list",
            Command::Info { .. } => "info",
            Command::Search { .. } => "search",
            Command::Provides { .. } => "provides",
            Command::CheckUpdate { .. } => "check-update",
            Command::Download { .. } => "download",
            Command::Makecache => "makecache",
            Command::Install { .. } => "install",
            Command::Remove { .. } => "remove",
            Command::Upgrade { .. } => "upgrade",
            Command::Clean { .. } => "clean",
        }
    }
}
