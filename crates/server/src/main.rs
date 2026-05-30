#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use sakurasato_core::Config;
use sakurasato_server::{cli, delivery, init, serve, token};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = cli::Cli::parse();

    let default_path: PathBuf = std::env::var_os("SAKURASATO_CONFIG")
        .unwrap_or_else(|| "config/default.toml".into())
        .into();

    let config = Config::load(&default_path, cli.config.as_deref())
        .with_context(|| format!("failed to load config from {}", default_path.display()))?;

    match cli.command {
        cli::Command::Serve => serve::run(config).await,
        cli::Command::Init(args) => init::run(config, args).await,
        cli::Command::Deliver(args) => delivery::run(config, args).await,
        cli::Command::Token(args) => token::run(config, args).await,
    }
}
