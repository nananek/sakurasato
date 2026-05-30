//! ローカル API (M4 PR1)。
//!
//! Unix socket 上で配信される TUI / 管理クライアント向けの内向き REST API。
//! 公開 TCP listener (`routes::router`) と **完全に別ルータ** に分離して
//! いる ── /api/v1/* を public 側に乗せないことで、reverse proxy 越しに
//! /api/v1/* が到達する可能性をルート定義レベルで断つ。
//!
//! 認証:
//! - ソケット mode 0600 が第一の壁 (`serve.rs` 参照)。
//! - Bearer トークンが第二の壁。CLI で発行し、SHA-256 hex を DB に格納。
//!   ([`crate::token`])
//!
//! ## ルート (M4 PR2 までに揃ったもの)
//!
//! - `GET /api/v1/whoami` ── 認証確認 + ローカル actor サマリ (PR1)
//! - `GET /api/v1/timeline/home` ── home timeline 一覧 (PR2)
//! - `POST /api/v1/notes` ── Note 作成 + Create Activity 配送 (PR2)
//! - `GET /api/v1/stream` ── SSE で新規 Note を購読 (PR2)

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use tokio::net::UnixListener;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::state::AppState;

pub mod auth;
pub mod notes;
pub mod stream;
pub mod timeline;
pub mod whoami;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/whoami", get(whoami::handle))
        .route("/api/v1/timeline/home", get(timeline::home))
        .route("/api/v1/notes", post(notes::create))
        .route("/api/v1/stream", get(stream::handle))
        // 全 `/api/v1/*` に Bearer 認証を要求する。`from_fn_with_state` で
        // middleware に `AppState` を渡し、`api_token` lookup に使う。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Unix socket を bind し、認証境界として安全な状態にして返す。
///
/// **二重ロック方針**:
/// 1. 親ディレクトリは `0o700` ── 同 UID 以外は中に入れない (= ソケット
///    エントリの存在自体を見せない)。
/// 2. ソケット本体は `0o600` ── 同 UID プロセスだけが read/write 可。
///
/// `bind()` と `chmod()` の間に微小な race window があるが、(1) の親 0o700 が
/// その間も他 UID プロセスを締め出すので実害は無い。親が既存のマウント
/// ポイントで chmod できない場合は warn だけ残してソケット側の 0o600 に
/// 頼る (compose では `/run/sakurasato` を server コンテナ専用 volume と
/// して掘る想定なので、通常はここで成功する)。
///
/// 古いソケットファイル (前回プロセスが汚いシャットダウンで残した) は
/// 黙って `unlink` する。ファイル以外 (regular file 等) が同パスにあった
/// 場合は `unlink` がエラーを返すので、人間が気付ける。
pub async fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir for {}", path.display()))?;
        let parent_perms = std::fs::Permissions::from_mode(0o700);
        if let Err(err) = std::fs::set_permissions(parent, parent_perms) {
            // mount point owned by another user 等で chmod 不能なケース。
            // ソケット側 0o600 が最後の壁になるので fatal にはしない。
            warn!(?err, parent = %parent.display(),
                "failed to chmod parent dir to 0o700; relying on socket mode");
        }
    }

    // 古いソケットを掃除。NotFound は無視するが、それ以外のエラーは fatal
    // (regular file が居座っているのを上書きしないため)。
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("remove stale socket at {}", path.display()));
        }
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind unix socket at {}", path.display()))?;
    // bind 直後に必ず 0o600 を打つ。
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 0o600 on socket {}", path.display()))?;

    Ok(listener)
}
