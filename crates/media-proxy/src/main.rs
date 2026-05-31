#![forbid(unsafe_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sakurasato_core::Config;
use sakurasato_media_proxy::{ProxyState, router};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing::{info, warn};
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

    // `--healthcheck` モード ── distroless image には shell も `test` も無いので、
    // docker healthcheck から呼べる自前のチェッカを binary に同居させる。
    // socket ファイルが存在すれば 0、無ければ 1 を返す ── `bind_socket` は serve
    // ループに入る **前** に完走するので、socket の存在 = listener が accept 可能。
    // これにより compose 側で `condition: service_healthy` を使って依存サービス
    // (例: `follow` CLI を回す prefollow-bob) を **ソケット readiness** に対して
    // 待たせられる。
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        let path = config.media_proxy.socket;
        if path.exists() {
            return Ok(());
        }
        eprintln!("media-proxy socket not found at {}", path.display());
        std::process::exit(1);
    }

    let socket_path = config.media_proxy.socket.clone();
    let state = ProxyState::from_config(config)?;
    let app = router(state.clone());

    let listener = bind_socket(&socket_path)
        .await
        .with_context(|| format!("bind media-proxy socket {}", socket_path.display()))?;

    info!(
        socket = %socket_path.display(),
        max_bytes = state.config().media_proxy.max_bytes,
        max_pixels = state.config().media_proxy.max_pixels,
        "sakurasato-media-proxy listening (UDS)"
    );

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.wait_for(|x| *x).await;
    });

    let result = serve.await;
    signal_task.abort();

    // 自身が unlink して終わる ── 次回起動で stale 検出されないように。
    if let Err(err) = tokio::fs::remove_file(&socket_path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?err, socket = %socket_path.display(),
            "failed to clean up media-proxy socket on shutdown");
    }

    info!("media-proxy shutdown complete");
    result.map_err(|e| anyhow::anyhow!("axum serve: {e}"))
}

async fn wait_for_shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => info!("media-proxy: SIGTERM received"),
        _ = int.recv() => info!("media-proxy: SIGINT received"),
    }
}

/// Unix socket を bind し、0o660 で開く。
///
/// **server との二重ロック**:
/// 1. 親ディレクトリは compose で `media_sock` named volume として server /
///    media-proxy 両方にだけマウントされる ── 他コンテナから到達不可。
/// 2. ソケット本体は `0o660` ── 同 group (= compose の docker-managed group)
///    プロセスだけが read/write 可。両 distroless `nonroot` user で動かす
///    ので、両者が同じ UID/GID に揃っていることが前提。
///
/// 古い socket ファイルは黙って unlink する (前回プロセスの異常終了対策)。
async fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }

    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("remove stale socket at {}", path.display()));
        }
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind unix socket at {}", path.display()))?;
    // 0o660: owner + group が read/write 可。other は不可。
    // server <-> media-proxy は同 UID/GID 前提 (compose 側で揃える)。
    let perms = std::fs::Permissions::from_mode(0o660);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 0o660 on socket {}", path.display()))?;
    Ok(listener)
}
