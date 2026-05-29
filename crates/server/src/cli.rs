use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "sakurasato-server",
    about = "Sakurasato — お一人様 Fediverse server"
)]
pub struct Cli {
    /// Path to an additional config TOML overlay (loaded on top of
    /// `config/default.toml`).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the AP federation daemon (HTTP + local Unix socket).
    Serve,
    /// Bootstrap the single local user actor and instance.actor.
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Override the username from `config.server.user` (advanced).
    #[arg(long)]
    pub username: Option<String>,
    /// Display name for the local actor (defaults to the username).
    #[arg(long)]
    pub display_name: Option<String>,
    /// Re-initialise even if a local actor already exists (re-issues the
    /// signing key — break federation, destructive).
    #[arg(long, default_value_t = false)]
    pub force: bool,
}
