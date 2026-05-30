use anyhow::Context;
use sakurasato_core::{Config, MIGRATOR};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::delivery;
use crate::local_api;
use crate::routes;
use crate::state::AppState;

/// Bring up the public HTTP listener (TCP) and the local API listener
/// (Unix socket), run pending migrations, spawn the resident delivery
/// worker, and serve until a SIGINT/SIGTERM is received.
///
/// 公開 TCP listener とローカル Unix socket listener は **完全に別のルータ**
/// を載せる:
/// - public: `routes::router` — AP 連合 + 最小 Web (permalink / media)
/// - local : `local_api::router` — TUI 向け `/api/v1/*` (Bearer 認証)
///
/// `/api/v1/*` は public listener のルートテーブルに存在しないため、
/// reverse proxy 経由で外部から到達するパスがそもそも無い (深層防御)。
pub async fn run(config: Config) -> anyhow::Result<()> {
    let bind = config.server.bind.clone();
    let socket_path = config.server.local_api_socket.clone();
    let state = AppState::from_config(config).await?;

    MIGRATOR
        .run(state.pool())
        .await
        .context("apply pending DB migrations")?;

    // shutdown 信号を worker / 公開 listener / ローカル listener で共有。
    // `watch::channel(false)` 初期 false、SIGTERM/SIGINT で true に倒し、
    // 各 graceful_shutdown フューチャが `wait_for(|x| *x)` で発火する。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let worker = delivery::worker::spawn(state.clone(), shutdown_rx.clone());

    let public_app = routes::router(state.clone());
    let local_app = local_api::router(state.clone());

    let public_listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    let local_listener = local_api::bind_socket(&socket_path)
        .await
        .with_context(|| format!("bind local API socket {}", socket_path.display()))?;

    info!(addr = %bind, "sakurasato-server listening (public TCP)");
    info!(socket = %socket_path.display(), "sakurasato-server listening (local API)");

    let mut public_shutdown_rx = shutdown_rx.clone();
    let mut local_shutdown_rx = shutdown_rx.clone();

    let public_serve =
        axum::serve(public_listener, public_app).with_graceful_shutdown(async move {
            let _ = public_shutdown_rx.wait_for(|x| *x).await;
        });
    let local_serve = axum::serve(local_listener, local_app).with_graceful_shutdown(async move {
        let _ = local_shutdown_rx.wait_for(|x| *x).await;
    });

    // signal を受けたら watch を true に倒し、両方の graceful shutdown を
    // 同時に発火させる。signal handler が両方を起こすので main は
    // `tokio::try_join!` で待つだけでよい。
    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let result = tokio::try_join!(public_serve, local_serve);

    // signal task は通常 shutdown_tx.send で終わるが、念のため abort して
    // 漏れがないようにする。SIGTERM を受け取った時点で `send` は完了して
    // いるはずなので abort 自体は no-op になることが多い。
    signal_task.abort();

    // axum シャットダウン後に worker の完走を待つ。worker は in-flight 配送
    // が終わってから終了するため、ここでの await はせいぜい 1 配送ぶん
    // (HTTP timeout 30s + α)。
    info!("waiting for delivery worker to drain in-flight deliveries");
    if let Err(err) = worker.await {
        warn!(error = %err, "delivery worker join failed");
    }

    // ソケットファイルの掃除。落ちたまま残しておくと次回起動で stale 扱い
    // されるが、`bind_socket` 側でも掃除するので best-effort で十分。
    if let Err(err) = tokio::fs::remove_file(&socket_path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?err, socket = %socket_path.display(),
            "failed to clean up local API socket on shutdown");
    }

    info!("shutdown complete");
    match result {
        Ok(_) => Ok(()),
        Err(err) => Err(anyhow::anyhow!("axum serve: {err}")),
    }
}

async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => info!("SIGTERM received"),
        _ = int.recv() => info!("SIGINT received"),
    }
}
