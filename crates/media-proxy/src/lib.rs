//! Sakurasato media-proxy: 隔離コンテナの実装本体。
//!
//! CLAUDE.md §5.3 / §7 で「外部 URL 取得と画像デコードはこのコンテナでだけ」
//! と定めた責務を、Unix socket 上の小さな HTTP API として実装する。
//!
//! # エンドポイント
//!
//! - `POST /v1/image/fetch` — リモート URL から画像を取得 / デコード / 指定
//!   バリアントへリサイズ / `WebP` 再エンコード。レスポンスは安全化済みの
//!   バイト列。
//! - `POST /v1/image/sanitize` — アップロード由来の生バイト列を受け取り、
//!   再エンコードで埋め込みペイロードと EXIF を落として返す。M7 (TUI からの
//!   アイコン/添付アップロード) で使う。
//! - `POST /v1/video/sanitize` — アップロード由来の動画バイト列を受け取り、
//!   コンテナメタデータ (udta/meta/uuid, Tags/Attachments/Chapters 等) を
//!   インプレース無害化して返す (再エンコードはしない)。
//! - `POST /v1/webfinger/resolve` — `acct:user@host` から `ActivityPub` actor
//!   URI を解決する (M10)。WebFinger 取得自体は JSON 通信だが、外向き接続を
//!   media-proxy に寄せて server コンテナの egress を絞る。
//! - `GET /healthz` — Liveness 用。常に `200 OK`。
//!
//! # 設計方針
//!
//! - `axum::serve` の listener は **Unix socket** のみ。本コンテナは TCP を
//!   開かない (`docker-compose.yml` の内部ネットでも `expose` しない)。
//! - サイズ上限: `max_bytes` (config) でダウンロード / 受信本文を頭打ち。
//! - SSRF: [`sakurasato_core::net_guard::host_blocked`] と redirect ごとの
//!   再検証を [`http_client`] にまとめる。
//! - 出力フォーマット: `image/webp`。EXIF などのメタデータは入らない。
//! - エラー JSON は [`error::ApiError`] で統一する (`{ "error": "...", "reason": "..." }`)。

#![forbid(unsafe_code)]

pub mod error;
pub mod fetch;
pub mod http_client;
pub mod image_pipeline;
pub mod sanitize;
pub mod state;
pub mod video_pipeline;
pub mod video_sanitize;
pub mod webfinger;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

pub use state::ProxyState;

/// Sakurasato media-proxy のルータを構築する。
///
/// `state` は出来上がった [`ProxyState`] を `Arc` で共有する。テストは
/// 同関数を直接呼び、`tower::ServiceExt::oneshot` で叩く。
pub fn router(state: Arc<ProxyState>) -> Router {
    // axum の `DefaultBodyLimit` は明示上書きしない限り 2 MiB。`max_bytes` /
    // `max_video_bytes` は config 由来でそれより大きいのが通常 (動画は
    // 既定 200 MiB) なので、各サニタイズ route に個別で override をかける
    // ── しないとハンドラ内の上限チェックに到達する前に axum 層で 413 になる。
    let image_body_limit = state.max_bytes();
    let video_body_limit = state.max_video_bytes();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/image/fetch", post(fetch::handle))
        .route(
            "/v1/image/sanitize",
            post(sanitize::handle).layer(axum::extract::DefaultBodyLimit::max(image_body_limit)),
        )
        .route(
            "/v1/video/sanitize",
            post(video_sanitize::handle)
                .layer(axum::extract::DefaultBodyLimit::max(video_body_limit)),
        )
        .route("/v1/webfinger/resolve", post(webfinger::handle))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `GET /healthz` — 常に `200 OK`。本体 (server) からの起動検査用。
async fn healthz() -> &'static str {
    "ok"
}
