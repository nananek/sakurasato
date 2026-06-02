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
//! ## ルート
//!
//! - `GET /api/v1/whoami` ── 認証確認 + ローカル actor サマリ (M4 PR1)
//! - `GET /api/v1/actor?acct=|ap_id=` ── actor 解決 + ローカルとの関係を返す
//!   (M13 PR1 / Issue #79)。`acct` 経路は `WebFinger` host 一致検証を経る。
//! - `GET /api/v1/actor/{id}` ── DB id で actor を取得 (M13 PR1)
//! - `GET /api/v1/actor/{id}/relationship` ── ローカル → target の follow 関係 (M13 PR1)
//! - `GET /api/v1/timeline/home` ── home timeline 一覧 (M4 PR2)
//! - `POST /api/v1/notes` ── Note 作成 + Create Activity 配送 (M4 PR2)
//! - `GET /api/v1/stream` ── SSE で新規 Note を購読 (M4 PR2)
//! - `GET /api/v1/media/proxy?url=&variant=` ── TUI 用画像プロキシ。
//!   media-proxy 経由で外部 URL を取得 (M6, Issue #36 解消)
//! - `POST /api/v1/media?kind=&alt=` ── 画像アップロード。media-proxy で
//!   サニタイズ後 versitygw に格納し、`media` 行を作る (M7)
//! - `PATCH /api/v1/actor/profile` ── アバター/ヘッダ/表示名等の更新 +
//!   Update Activity 配送 (M7)
//! - `POST /api/v1/reactions` ── ローカル Note へのリアクション作成 + EmojiReact/Like
//!   配送 (M8 PR2)
//! - `DELETE /api/v1/reactions/{id}` ── 自分のリアクション取消 + Undo 配送 (M8 PR2)
//! - `POST /api/v1/actor/lock` / `POST /api/v1/actor/unlock` ── 鍵アカフラグ
//!   切替 + actor Update 配信 (M12 / Issue #66)
//! - `GET /api/v1/follow-requests` ── 承認待ち follow 一覧 (M12 / Issue #66)
//! - `POST /api/v1/follow-requests/{id}/approve` ── Accept 配送 + state 遷移
//! - `POST /api/v1/follow-requests/{id}/reject` ── Reject 配送 + state 遷移
//! - `POST /api/v1/follow` ── Follow を `delivery_queue` に投入 (M13 PR2 / Issue #79)
//! - `DELETE /api/v1/follow/{id}` ── Undo Follow 送出 + follow 行削除 (M13 PR2)
//! - `GET /api/v1/following` ── 自分が follow している accepted 一覧 (M13 PR3)
//! - `GET /api/v1/followers` ── 自分を follow している accepted 一覧 (M13 PR3)
//! - `GET /api/v1/actor/{id}/notes` ── 当該 actor の Note 一覧。viewer 視点の
//!   visibility filter 経由 (M13 PR3)

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use tokio::net::UnixListener;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::state::AppState;

pub mod actor;
pub mod actor_admin;
pub mod auth;
pub mod emojis;
pub mod follow;
pub mod follow_list;
pub mod follow_request;
pub mod media;
pub mod media_proxy;
pub mod notes;
pub mod profile;
pub mod reactions;
pub mod renotes;
pub mod stream;
pub mod timeline;
pub mod whoami;

pub fn router(state: AppState) -> Router {
    // M7: 画像アップロードはサニタイズ前段で media-proxy.max_bytes に達する
    // 想定の大きいバイト列を受ける。axum の DefaultBodyLimit (= 2 MiB) を
    // 当該ルートだけ拡張する。`media.upload` ハンドラ自身も上限を再確認
    // するので、ここは max_bytes と同じ値に揃えればよい。
    let upload_max = usize::try_from(state.config().media_proxy.max_bytes).unwrap_or(usize::MAX);

    Router::new()
        .route("/api/v1/whoami", get(whoami::handle))
        // M13 PR1 (Issue #79): リモート/ローカル actor の lookup + relationship。
        // TUI の Profile 画面と `:follow` コマンドの基盤。
        .route("/api/v1/actor", get(actor::lookup))
        .route("/api/v1/actor/{id}", get(actor::get_by_id))
        .route("/api/v1/actor/{id}/relationship", get(actor::relationship))
        // M13 PR3 (Issue #79): Profile 画面下部の「最近の投稿」+ FollowList。
        // visibility filter は viewer (= ローカル actor) 視点で評価する。
        .route("/api/v1/actor/{id}/notes", get(actor::list_notes))
        .route("/api/v1/following", get(follow_list::following))
        .route("/api/v1/followers", get(follow_list::followers))
        .route("/api/v1/timeline/home", get(timeline::home))
        .route("/api/v1/notes", post(notes::create))
        .route("/api/v1/stream", get(stream::handle))
        // M6: TUI 用画像プロキシ。media-proxy 経由で外部 URL を取得する。
        .route("/api/v1/media/proxy", get(media_proxy::handle))
        // M7: メディアアップロード。サニタイズ済みバイト列を versitygw に
        // 格納し、`media` 行を作る。`DefaultBodyLimit::max` でこのルートだけ
        // 上限を拡張する (= 他ルートは 2 MiB 既定のまま)。
        .route(
            "/api/v1/media",
            post(media::upload).layer(axum::extract::DefaultBodyLimit::max(upload_max)),
        )
        // M7: 自プロフィール (display_name / summary / icon / image) の更新と
        // Update Activity 連合送出。
        .route(
            "/api/v1/actor/profile",
            axum::routing::patch(profile::patch),
        )
        // M8: ローカル user が自分の Note にリアクションを付けて連合先に通知。
        // POST = 作成 (EmojiReact / Like)、DELETE = 取り消し (Undo)。
        .route("/api/v1/emojis", get(emojis::list))
        .route("/api/v1/reactions", post(reactions::create))
        .route(
            "/api/v1/reactions/{id}",
            axum::routing::delete(reactions::delete),
        )
        // #151: Announce (boost / renote) 送出。POST = boost、DELETE = Undo。
        // path の `id` は **元 Note の id** (受信側 announce 行の id ではない)。
        // 1 user 1 target 制約は announce テーブルの UNIQUE で担保される。
        .route(
            "/api/v1/notes/{id}/renote",
            post(renotes::create).delete(renotes::delete),
        )
        // M12 / Issue #66: 鍵アカ運用の lock/unlock + 承認待ち管理。
        // CLI と同じロジックを呼ぶだけ。pytest 連合テストと TUI 共通のフロント。
        .route("/api/v1/actor/lock", post(actor_admin::lock))
        .route("/api/v1/actor/unlock", post(actor_admin::unlock))
        .route("/api/v1/follow-requests", get(follow_request::list))
        .route(
            "/api/v1/follow-requests/{id}/approve",
            post(follow_request::approve),
        )
        .route(
            "/api/v1/follow-requests/{id}/reject",
            post(follow_request::reject),
        )
        // PR #80 round-2 #6: 古いフォロー行をハード削除 (test cleanup 用)。
        .route(
            "/api/v1/follow-requests/{id}",
            axum::routing::delete(follow_request::delete),
        )
        // M13 PR2 (Issue #79): TUI Profile 画面 / `:follow` コマンドが叩く。
        // `POST` は冪等 (既存 accepted は no-op で 200 を返す)、
        // `DELETE` は本人の follow のみ削除可能 (= 403 ガード)。
        .route("/api/v1/follow", post(follow::create))
        .route("/api/v1/follow/{id}", axum::routing::delete(follow::delete))
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
