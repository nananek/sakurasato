use std::path::PathBuf;

use anyhow::Context;
use axum::Router;
use sakurasato_core::{Config, Listen, MIGRATOR};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::delivery;
use crate::local_api;
use crate::miauth;
use crate::routes;
use crate::state::AppState;

/// Bring up the public HTTP listener and the local API listener (each
/// independently TCP or UDS per [`Listen`] config), run pending migrations,
/// spawn the resident delivery worker, and serve until a SIGINT/SIGTERM
/// is received.
///
/// 公開 listener とローカル listener は **完全に別のルータ** を載せる:
/// - public: `routes::router` — AP 連合 + 最小 Web (permalink / media)
/// - local : `local_api::router` — TUI 向け `/api/v1/*` (Bearer 認証)
///
/// `/api/v1/*` は public listener のルートテーブルに存在しないため、
/// reverse proxy 経由で外部から到達するパスがそもそも無い (深層防御)。
///
/// **Transport 切替** (2026-05-31 #69):
/// - `server.public_listen` URI で公開側を `tcp://` / `unix:/` 選択。
///   Cloudflared Tunnel 経由なら `unix:/run/sakurasato/public.sock` 推奨
///   (ポートを host に晒さない)。未設定なら `server.bind` を TCP fallback。
/// - `server.local_api_listen` URI で TUI 側を `unix:/` / `tcp://` 選択。
///   Tailscale 経由で別端末から TUI を触るなら `tcp://0.0.0.0:18080` 推奨。
///   未設定なら `server.local_api_socket` を UDS fallback。
pub async fn run(config: Config) -> anyhow::Result<()> {
    let public_listen = config.server.public_listener()?;
    let local_listen = config.server.local_api_listener()?;
    // M14 #157: optional MiAuth listener (= 親 issue #150)。設定が無ければ
    // socket を作らず、Misskey クライアント互換 endpoint も一切公開しない
    // (= 既存 deploy にゼロ影響)。設定があれば listen URI を解釈し、3 つ目
    // の serve_role task として spawn する。
    let miauth_listen = config
        .miauth
        .as_ref()
        .map(sakurasato_core::config::MiAuthConfig::listener)
        .transpose()?;
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

    let public_uds_for_cleanup = uds_path_for_cleanup(&public_listen);
    let local_uds_for_cleanup = uds_path_for_cleanup(&local_listen);
    let miauth_uds_for_cleanup = miauth_listen.as_ref().and_then(uds_path_for_cleanup);

    info!(role = "public", listen = %public_listen.display(), "sakurasato-server starting listener");
    info!(role = "local-api", listen = %local_listen.display(), "sakurasato-server starting listener");
    if let Some(ref listen) = miauth_listen {
        info!(role = "miauth", listen = %listen.display(), "sakurasato-server starting listener");
        warn_if_non_loopback_miauth(listen);
    }
    warn_if_non_loopback_local_api(&local_listen);

    let public_fut = serve_role(
        ListenerRole::Public,
        public_listen,
        public_app,
        shutdown_rx.clone(),
    );
    let local_fut = serve_role(
        ListenerRole::LocalApi,
        local_listen,
        local_app,
        shutdown_rx.clone(),
    );

    // signal を受けたら watch を true に倒し、両方の graceful shutdown を
    // 同時に発火させる。signal handler が両方を起こすので main は
    // `tokio::try_join!` で待つだけでよい。
    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    // MiAuth listener は **optional**。設定があるときだけ serve_role を
    // try_join! の対象に追加する。両方の serve future が結果的に `Ok(())`
    // を返せばよいので、無いときは `async { Ok(()) }` の薄い filler を使う。
    let miauth_app = miauth::router(state.clone());
    let miauth_fut = async move {
        match miauth_listen {
            Some(listen) => {
                serve_role(
                    ListenerRole::MiAuth,
                    listen,
                    miauth_app,
                    shutdown_rx.clone(),
                )
                .await
            }
            None => Ok(()),
        }
    };

    let result = tokio::try_join!(public_fut, local_fut, miauth_fut);

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

    // ソケットファイルの掃除 (UDS 経路でのみ)。落ちたまま残しておくと次回
    // 起動で stale 扱いされるが、`bind_socket` 側でも掃除するので
    // best-effort で十分。
    for (label, path) in [
        ("public", public_uds_for_cleanup),
        ("local-api", local_uds_for_cleanup),
        ("miauth", miauth_uds_for_cleanup),
    ] {
        let Some(path) = path else { continue };
        if let Err(err) = tokio::fs::remove_file(&path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(?err, role = label, socket = %path.display(),
                "failed to clean up unix socket on shutdown");
        }
    }

    info!("shutdown complete");
    match result {
        Ok(_) => Ok(()),
        Err(err) => Err(anyhow::anyhow!("axum serve: {err}")),
    }
}

/// listener の役割。`bind_socket` の chmod 戦略を分けるために使う ──
/// `LocalApi` / `MiAuth` は 0o600 厳格 (= Bearer + ファイル権限の二重壁)、
/// `Public` は 0o666 寛容 (= 同 compose 内の Cloudflared 等が同 volume 越し
/// に繋ぐ想定、AP レイヤ側で HTTP 署名検証する)。
#[derive(Debug, Clone, Copy)]
enum ListenerRole {
    Public,
    LocalApi,
    /// M14 #157: Misskey `MiAuth` 互換 API endpoint (= 親 issue #150)。
    /// socket 権限ポリシーは `LocalApi` と同じ (= `miauth::bind_socket` は
    /// `local_api::bind_socket` への delegation)。
    MiAuth,
}

impl ListenerRole {
    fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::LocalApi => "local-api",
            Self::MiAuth => "miauth",
        }
    }
}

/// 与えられた `Listen` に応じて `TcpListener` か `UnixListener` を bind し、
/// `axum::serve` を graceful shutdown 付きで回す。
async fn serve_role(
    role: ListenerRole,
    listen: Listen,
    app: Router,
    shutdown_rx: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut shutdown_rx = shutdown_rx;
    let label = role.label();
    match listen {
        Listen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .map_err(|e| std::io::Error::other(format!("bind {label} TCP {addr}: {e}")))?;
            info!(role = label, addr = %addr, "sakurasato-server listening (TCP)");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.wait_for(|x| *x).await;
                })
                .await
        }
        Listen::Unix(path) => {
            let listener = match role {
                ListenerRole::LocalApi => local_api::bind_socket(&path).await,
                ListenerRole::MiAuth => miauth::bind_socket(&path).await,
                ListenerRole::Public => bind_public_unix(&path).await,
            }
            .map_err(|e| {
                std::io::Error::other(format!("bind {label} UDS {}: {e:#}", path.display()))
            })?;
            info!(role = label, socket = %path.display(), "sakurasato-server listening (UDS)");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.wait_for(|x| *x).await;
                })
                .await
        }
    }
}

/// 公開 UDS 用の bind ヘルパ。`local_api::bind_socket` (0o600 + 0o700) と
/// 異なり、socket は 0o666 で開けて Cloudflared 等の別 UID プロセスが同
/// volume 越しに繋げるようにする。認証は AP レイヤ (= HTTP 署名検証) が
/// 担うので、socket 自体のファイル権限を絞る意味は薄い。
async fn bind_public_unix(path: &std::path::Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }
    // 古いソケットを掃除 (NotFound は無視、それ以外は fatal)。
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::anyhow!(err))
                .with_context(|| format!("remove stale socket at {}", path.display()));
        }
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("bind public unix socket at {}", path.display()))?;
    // 0o666: world-rw。Cloudflared 等が別 uid で繋ぐ前提。socket 越しの
    // クライアント認証は不要 (= 公開 AP listener の本質的性質)。volume を
    // compose 内で隔離する設計に任せる。
    //
    // **[PR #70 round-2 #2]**: chmod は **bind 直後の同期 syscall** で打つ
    // ── async fs::set_permissions に変えると bind → await 境界 → chmod の
    // 間に TOCTOU 窓が生まれ、hardened umask 環境 (umask=0o177 等) では
    // socket が一瞬 0o600 で見え、別 UID の Cloudflared が EACCES を踏む。
    // `std::fs::set_permissions` は 1 syscall (`chmod`) なので Tokio thread
    // ブロック量は無視できる。`bind_socket` (= 0o600) と同じパターンに揃える。
    let perms = std::fs::Permissions::from_mode(0o666);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 0o666 on public socket {}", path.display()))?;
    Ok(listener)
}

/// `Listen::Tcp` の bind 先が非 loopback (= 0.0.0.0 / 公衆 IP) のとき、
/// オペレータに「ポート転送と Tailscale ACL を確認しろ」と警告する。
///
/// **[PR #70 round-2 #3]**: `local_api_listen = "tcp://0.0.0.0:18080"` を
/// 受理するが、Docker の `ports: "18080:18080"` (= 127.0.0.1 prefix 忘れ)
/// と組み合わさると local API がインターネット公開される。UDS 経路では
/// 0o600 がファイルシステム壁だったが、TCP には Bearer トークンしか壁が
/// ないので、起動時に明示警告で防御深度を一段上げる。
fn warn_if_non_loopback_local_api(listen: &Listen) {
    let Listen::Tcp(addr) = listen else {
        return;
    };
    let is_loopback = addr
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|sa| sa.ip().is_loopback());
    if !is_loopback {
        warn!(
            addr = %addr,
            "local API is bound to a non-loopback TCP address. \
             Ensure docker `ports:` uses 127.0.0.1: prefix and Tailscale ACL is configured. \
             (see DEPLOYMENT.md §5.1)"
        );
    }
}

/// M14 #157: `MiAuth` listener が非 loopback の TCP に bind された場合の警告。
/// Tailscale tailnet を介して mobile から叩く前提の構成では `0.0.0.0` 経由が
/// 必要だが、`ports:` で host ポートを 127.0.0.1 prefix なしで露出するとイン
/// ターネットに **Misskey 互換 endpoint がそのまま公開** されることになる。
/// `MiAuth` は permission scope を強制するが、ハイジャックされた token で投稿
/// まで通る (= `write:notes` 持ち token なら誰でも投稿可能) ので、多層防御
/// として起動時に明示警告で防御深度を一段上げる (= `local_api` と同じ rationale)。
fn warn_if_non_loopback_miauth(listen: &Listen) {
    let Listen::Tcp(addr) = listen else {
        return;
    };
    let is_loopback = addr
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|sa| sa.ip().is_loopback());
    if !is_loopback {
        warn!(
            addr = %addr,
            "MiAuth listener is bound to a non-loopback TCP address. \
             Ensure docker `ports:` uses 127.0.0.1: prefix and Tailscale ACL is configured. \
             Cloudflared / nginx must NOT proxy this socket — Misskey-compatible \
             endpoints accept token-bearer write operations including post creation. \
             (see DEPLOYMENT.md MiAuth section)"
        );
    }
}

/// `Listen::Unix` であればその path を返す (shutdown 時の cleanup 用)。
fn uds_path_for_cleanup(listen: &Listen) -> Option<PathBuf> {
    match listen {
        Listen::Unix(p) => Some(p.clone()),
        Listen::Tcp(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// **[PR #70 round-2 #3]**: loopback bind は警告しない、非 loopback は
    /// 警告する。実際の `warn!` 発火は tracing-test を引かないと検証できない
    /// ので、`is_loopback` 判定が想定どおり動くかを `SocketAddr` 経路で確認。
    #[test]
    fn warn_if_non_loopback_local_api_only_fires_on_non_loopback() {
        // どれも panic せずに通ること (= 関数自体の sanity)。
        warn_if_non_loopback_local_api(&Listen::Tcp("127.0.0.1:18080".into()));
        warn_if_non_loopback_local_api(&Listen::Tcp("[::1]:18080".into()));
        warn_if_non_loopback_local_api(&Listen::Tcp("0.0.0.0:18080".into()));
        warn_if_non_loopback_local_api(&Listen::Tcp("[::]:18080".into()));
        // hostname は SocketAddr parse できないので「loopback でない」扱い
        // → 警告される。ここでも panic しないことだけ確認。
        warn_if_non_loopback_local_api(&Listen::Tcp("localhost:18080".into()));
        // UDS は対象外で no-op。
        warn_if_non_loopback_local_api(&Listen::Unix(PathBuf::from("/run/local.sock")));
    }

    /// IP literal の loopback 判定が `is_loopback` で正しく拾えること
    /// (= 警告の必要性判定ロジックの単体検証)。
    #[test]
    fn is_loopback_detection_matches_std() {
        let cases = [
            ("127.0.0.1:18080", true),
            ("127.5.5.5:18080", true), // 127.0.0.0/8 全部
            ("[::1]:18080", true),
            ("0.0.0.0:18080", false),
            ("[::]:18080", false),
            ("10.0.0.1:18080", false),
        ];
        for (addr, expected) in cases {
            let parsed = addr.parse::<std::net::SocketAddr>().unwrap();
            assert_eq!(
                parsed.ip().is_loopback(),
                expected,
                "addr {addr} loopback judgment",
            );
        }
    }
}
