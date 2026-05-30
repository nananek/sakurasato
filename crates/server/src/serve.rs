use anyhow::Context;
use sakurasato_core::{Config, MIGRATOR};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing::info;

use crate::delivery;
use crate::routes;
use crate::state::AppState;

/// Bring up the HTTP listener, run pending migrations, spawn the resident
/// delivery worker, and serve until a SIGINT/SIGTERM is received.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let bind = config.server.bind.clone();
    let state = AppState::from_config(config).await?;

    MIGRATOR
        .run(state.pool())
        .await
        .context("apply pending DB migrations")?;

    // shutdown 信号を worker と axum で共有。`watch::channel(false)` で
    // 初期値 false、SIGTERM/SIGINT で true に倒す。worker は `borrow()` /
    // `changed()` で受け取り、in-flight 配送を完走させてから終了する。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let worker = delivery::worker::spawn(state.clone(), shutdown_rx);

    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    info!(addr = %bind, "sakurasato-server listening");

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // axum シャットダウン開始と同時に worker にも通知。
            let _ = shutdown_tx.send(true);
        })
        .await
        .context("axum serve");

    // axum シャットダウン後に worker の完走を待つ。worker は in-flight 配送
    // が終わってから終了するため、ここでの await はせいぜい 1 配送ぶん
    // (HTTP timeout 30s + α)。
    info!("waiting for delivery worker to drain in-flight deliveries");
    if let Err(err) = worker.await {
        tracing::warn!(error = %err, "delivery worker join failed");
    }
    info!("shutdown complete");
    serve_result
}

async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => info!("SIGTERM received"),
        _ = int.recv() => info!("SIGINT received"),
    }
}
