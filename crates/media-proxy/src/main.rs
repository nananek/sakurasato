#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Context;
use sakurasato_core::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let default_path: PathBuf = std::env::var_os("SAKURASATO_CONFIG")
        .unwrap_or_else(|| "config/default.toml".into())
        .into();

    let config = Config::load(&default_path, None)
        .with_context(|| format!("failed to load config from {}", default_path.display()))?;

    tracing::info!(
        socket = %config.media_proxy.socket.display(),
        max_bytes = config.media_proxy.max_bytes,
        "sakurasato-media-proxy starting (M1 skeleton); awaiting SIGINT/SIGTERM"
    );

    // M6 で取得/変換/OGP/サニタイズ本体を実装する。M1 では設定ロード確認と
    // signal を受けたら正常終了するだけのループ。
    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, exiting");
    Ok(())
}

async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
}
