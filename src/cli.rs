//! CLI argument parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fleet-commander", about = "Orchestrate AI coding agents via ACP")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
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
