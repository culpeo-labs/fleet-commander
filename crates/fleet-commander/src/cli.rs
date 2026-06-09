//! CLI argument parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fleet-commander",
    about = "Orchestrate AI coding agents via ACP"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Append every ACP wire message (both directions) to the given file,
    /// one line per message prefixed with `>>` (sent) or `<<` (received).
    /// Useful for debugging protocol issues.
    #[arg(long, global = true, value_name = "FILE")]
    pub acp_log: Option<PathBuf>,

    /// Only log wire messages for agents whose id contains this substring.
    /// Useful when running multiple agents and you want to follow a single
    /// one. No effect unless `--acp-log` is also set.
    #[arg(long, global = true, value_name = "PATTERN", requires = "acp_log")]
    pub acp_log_filter: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a workspace: select agent, scan for devcontainer projects, generate credential layer.
    Init {
        /// Path to the workspace root (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}
