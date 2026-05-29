use anyhow::Context;
use sakurasato_core::{Config, MIGRATOR};
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

use crate::routes;
use crate::state::AppState;

/// Bring up the HTTP listener, run pending migrations, and serve until a
/// SIGINT/SIGTERM is received.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let bind = config.server.bind.clone();
    let state = AppState::from_config(config).await?;

    MIGRATOR
        .run(state.pool())
        .await
        .context("apply pending DB migrations")?;

    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    info!(addr = %bind, "sakurasato-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => info!("SIGTERM received"),
        _ = int.recv() => info!("SIGINT received"),
    }
}
