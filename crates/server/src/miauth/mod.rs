//! Misskey `MiAuth` 互換 API endpoint の **基盤レイヤ** (M14 #157, 親 issue #150)。
//!
//! 既存 `local_api` (= TUI 向け Mastodon 風 path + Bearer 認証) と **別 listener**
//! で公開する設計。本 module ツリーが乗る socket は `config.miauth.listen` で
//! 切替可能で、未設定なら socket 自体が作られない (= デフォルトでは `MiAuth`
//! 機能は完全に無効、既存 deploy への影響ゼロ)。
//!
//! ## AGPL discipline (= [[agpl-discipline-miauth]] / #150 description)
//!
//! Misskey 本体は AGPL-3.0 (§13 network copyleft)、Sakurasato は MIT。本実装は
//! [misskey-hub.net](https://misskey-hub.net/) + [api-doc.misskey.io](https://api-doc.misskey.io/)
//! の **公開 API 仕様のみ** を一次資料とした clean-room implementation で、
//! Misskey の TypeScript handler を読まずに書く (API 仕様は interface =
//! 著作権対象外、Oracle v Google)。動作の細部確認が必要な場合は misskey-py
//! (= MIT, [YuzuRyo61/Misskey.py](https://github.com/YuzuRyo61/Misskey.py)) で
//! `curl` 相当の観察を行い、観察結果のみ反映する。
//!
//! ## #157 (foundation) スコープ
//!
//! 本 PR では DB スキーマ + config + listener + auth ヘルパ + CLI までを整備し、
//! 実 endpoint は `/healthz` だけを生やす。session register / check / `/api/i` は
//! #158、read endpoints (`timeline` / `show` / `emojis` / `users`) は #159、
//! write endpoints (`notes/create` / `reactions/*` / `following/*`) は #160 で
//! 順次追加する。

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use std::path::Path;
use tokio::net::UnixListener;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod auth;
pub mod check;
pub mod conv;
pub mod drive;
pub mod emojis;
pub mod endpoints;
pub mod error;
pub mod following;
pub mod i;
pub mod meta;
pub mod nodeinfo;
pub mod notes;
pub mod notifications;
pub mod reactions;
pub mod session;
pub mod stats;
pub mod streaming;
pub mod text;
pub mod users;

/// `/healthz` レスポンス。listener が生きていることだけを示す liveness probe。
/// 認証不要 (= compose の healthcheck や Tailscale 越しの reachability test で
/// 叩く)。
async fn healthz() -> &'static str {
    "ok"
}

/// `MiAuth` 用 axum [`Router`]。`#[cfg(not(test))]` 等で切り替える必要は無く、
/// `config.miauth.is_some()` のときだけ [`crate::serve::run`] からこの router
/// が listener にぶら下げられる。
///
/// ## #158 で乗る endpoint
///
/// - `GET /miauth/{uuid}?name=&permission=&callback=` ── browser landing
///   (pending session 登録 + CLI 指示テキスト表示) ([`session::handle`])
/// - `POST /api/miauth/{uuid}/check` ── Misskey クライアント polling
///   ([`check::handle`])
/// - `POST /api/i` ── whoami (= 認証された token に紐付く `MissUser`)
///   ([`i::handle`])
///
/// 以降の `MiAuth` endpoint (= read endpoints: `notes/timeline` / `notes/show` /
/// `emojis` / `users/show`, write endpoints: `notes/create` / `reactions/*` /
/// `following/*`) は #159 / #160 で追加される。
pub fn router(state: AppState) -> Router {
    // drive/files/create は media-proxy.max_bytes 近くの大きい multipart を受ける。
    // local_api の upload route と同じく axum 既定 (2 MiB) ではなく max_bytes に
    // 揃える ── でないと大きい画像が size チェック前に axum で弾かれ、local_api と
    // 挙動が非対称になる (#218 review)。
    let upload_max = usize::try_from(state.config().media_proxy.max_bytes).unwrap_or(usize::MAX);
    Router::new()
        .route("/healthz", get(healthz))
        // M14 #168 ── instance probe (= client が login URL を入れた瞬間に叩く)
        .route("/.well-known/nodeinfo", get(nodeinfo::well_known))
        // 2.1 doc は AP listener と完全に同一 (= software/protocols/usage は
        // インスタンス全体で 1 つ)。MiAuth 経路からも同じ handler を呼べる
        // ように [`crate::routes::nodeinfo::v2_1`] を直接 mount する。
        .route("/nodeinfo/2.1", get(crate::routes::nodeinfo::v2_1))
        .route("/api/meta", post(meta::handle))
        .route("/api/stats", post(stats::handle))
        // M14 #176 ── /api/endpoints (Aria が emoji picker で使うか判定する)
        .route("/api/endpoints", post(endpoints::handle))
        // M14 #158 ── 認証フロー
        .route("/miauth/{uuid}", get(session::handle))
        .route("/api/miauth/{uuid}/check", post(check::handle))
        .route("/api/i", post(i::handle))
        // #206 PR2 ── in-app 通知フィード (Aria 通知タブ)
        .route("/api/i/notifications", post(notifications::list))
        .route(
            "/api/notifications/mark-all-as-read",
            post(notifications::mark_all_read),
        )
        // M14 #159 ── read endpoints
        .route("/api/notes/show", post(notes::show))
        .route("/api/notes/timeline", post(notes::timeline))
        .route("/api/emojis", post(emojis::handle))
        .route("/api/users/show", post(users::handle))
        // M14 #160 ── write endpoints
        .route("/api/notes/create", post(notes::create))
        .route("/api/notes/delete", post(notes::delete))
        .route("/api/notes/renote", post(notes::renote))
        .route("/api/notes/reactions/create", post(reactions::create))
        .route("/api/notes/reactions/delete", post(reactions::delete))
        .route("/api/following/create", post(following::create))
        .route("/api/following/delete", post(following::delete))
        // drive (= Aria の添付アップロード / ドライブ閲覧)
        .route(
            "/api/drive/files/create",
            post(drive::create).layer(axum::extract::DefaultBodyLimit::max(upload_max)),
        )
        .route("/api/drive/files", post(drive::list))
        .route("/api/drive/files/show", post(drive::show))
        .route("/api/drive/files/update", post(drive::update))
        .route("/api/drive/files/delete", post(drive::delete))
        // ドライブ閲覧 UI (= Aria のドライブタブ)。usage バー + フォルダ一覧 (空)。
        .route("/api/drive", post(drive::usage))
        .route("/api/drive/folders", post(drive::folders))
        // M14 #170 ── /streaming WebSocket stub (Aria UI の「接続中…」hang 回避)
        .route("/streaming", get(streaming::handle))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `MiAuth` UDS 用 bind。`local_api::bind_socket` と同じ 0o700 親ディレクトリ +
/// 0o600 socket 本体 (= 二重ロック)。`MiAuth` は Misskey クライアントから叩か
/// れる前提で「同 UID のプロセス間 IPC」ではなく「Tailscale 越しに mobile から
/// 叩く」可能性が高いが、内部仕様としては同 UID で完結する (= cloudflared に
/// 露出しないのが推奨運用、`DEPLOYMENT.md` 参照)。
///
/// 既存 `local_api::bind_socket` を再 export する形にしているのは、socket 権限
/// ポリシーが完全に一致するため (= コードを 2 重持ちにすると将来 race window
/// 対策をどちらか片方で打ち忘れる事故が起きる)。
pub async fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    crate::local_api::bind_socket(path)
        .await
        .with_context(|| format!("bind MiAuth UDS at {}", path.display()))
}
