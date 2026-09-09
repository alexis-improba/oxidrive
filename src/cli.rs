//! Command-line interface for the `oxidrive` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Root parser: global flags apply to every subcommand.
///
/// Use [`parse_args`] after startup to obtain a [`Cli`] value.
#[derive(Debug, Parser)]
#[command(name = "oxidrive", version, about)]
pub struct Cli {
    /// Path to the configuration file (TOML or JSON).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Show extra human-readable log lines (repeat for even more, less important, detail).
    #[arg(long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Only warnings and errors (same as the default; overrides a chatty config).
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// Primary entry points: OAuth setup, one-shot sync, service helper, and status.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize OAuth2 authentication with Google.
    Setup,
    /// Run sync (continuous daemon when configured, or single cycle with `--once`).
    Sync {
        /// Plan actions without modifying local or remote files.
        #[arg(long)]
        dry_run: bool,
        /// Force a single sync cycle even when daemon interval is configured.
        #[arg(long)]
        once: bool,
    },
    /// Manage the background sync service integration for this platform.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show sync diagnostics: configuration, state counters, uploads, and pending recovery ops.
    Status,
}

/// `service` subcommands for platform service integration.
#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Install the user systemd unit.
    Install,
    /// Remove the installed unit.
    Uninstall,
    /// Start the service.
    Start,
    /// Stop the service.
    Stop,
}

/// Parse CLI arguments from the environment.
pub fn parse_args() -> Cli {
    Cli::parse()
}
