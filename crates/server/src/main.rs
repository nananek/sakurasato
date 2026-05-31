#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use sakurasato_core::Config;
use sakurasato_server::{
    actor_admin, cli, delivery, emoji_import, follow, follow_request, init, move_accept, move_out,
    serve, token,
};
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
        cli::Command::Emoji(args) => emoji_import::run(config, args).await,
        cli::Command::Alias(args) => move_out::run_alias(config, args).await,
        cli::Command::MoveOut(args) => move_out::run_move_out(config, args).await,
        cli::Command::Follow(args) => follow::run(config, args).await,
        cli::Command::MoveAccept(args) => move_accept::run(config, args).await,
        cli::Command::Actor(args) => actor_admin::run(config, args).await,
        cli::Command::FollowRequest(args) => follow_request::run(config, args).await,
    }
}
