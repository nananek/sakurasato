//! `GET /media-proxy?url=...&variant=...` ── `MiAuth` 経路 (Aria 等) が返す
//! リモート origin の画像 URL を media-proxy 経由に橋渡しする、**無認証**の
//! 公開エンドポイント。
//!
//! ## 背景
//!
//! `MiAuth` (`crate::miauth`) の JSON レスポンスは、これまで remote actor の
//! `avatarUrl`/`bannerUrl` や remote Note の添付 `url`/`thumbnailUrl`、本文
//! `:emoji:` 画像を **相手サーバの生 URL のまま** 返していた。Misskey 互換
//! クライアント (Aria 等) の画像ウィジェットはそれを直接 GET するため、
//! media-proxy (SSRF ガード済み fetch + WebP 再エンコード + サイズ上限) を
//! 一切経由せず、危険なバイト列を Aria 端末が直接デコードすることになる
//! (= 報告された「`MiAuth` 経路では media-proxy が全く使われていない」問題)。
//!
//! `crate::miauth::conv` はこのエンドポイントの絶対 URL
//! (`https://{config.server.host}/media-proxy?url=<encoded>&variant=<v>`) を
//! 埋め込むことで、remote origin の画像を必ず media-proxy 経由に倒す。
//!
//! ## 配置: なぜ `MiAuth` 専用 socket ではなく公開 listener か
//!
//! `MiAuth` 自身の endpoint 群 (`/api/*`) は別 Unix socket 上にあり、
//! `DEPLOYMENT.md` §6 で **インターネットに直接公開してはいけない**
//! (Tailscale tailnet 越しのみ推奨) と定めている。画像 URL を仮にそちらに
//! 置くと、Aria が実際に画像を読み込めるかどうかが「`MiAuth` 到達時に使った
//! ネットワーク経路」に依存してしまう。
//!
//! 一方 `crate::miauth::conv` は元々すべての media URL
//! (`emoji_rows_to_url_map` / `media_row_to_miss_file` 等) を `/media/<key>`
//! と同じ **公開 AP host** (`config.server.host`, `routes::router` が listen)
//! で組み立てており、この host は通常のインターネット越しに Aria から常に
//! 到達可能という前提が既に成立している。本エンドポイントもこれに合わせて
//! `routes::router` (公開 listener) に置く。
//!
//! ## 無認証であることの帰結: open relay 化の防止
//!
//! `/media/<key>` (既存) は「自鯖が既に保存したオブジェクトだけを返す」ため
//! 無認証でも open relay にならないが、本エンドポイントは呼び出し元が
//! 任意の `url` を指定できる。無制限に許すと、当インスタンスを
//! 任意 URL 向け帯域 / SSRF プロービングの踏み台にされうる。対策:
//!
//! - [`sakurasato_core::net_guard::host_blocked`] による SSRF ガード
//!   (private/loopback/link-local/reserved 遮断、[`crate::media_proxy_route::validate`])。
//! - 宛先 host ごとの per-domain トークンバケット
//!   ([`AppState::try_acquire_media_proxy_fetch`])。AP object fetch 用の
//!   [`AppState::try_acquire_fetch`] とは **別バケット** ── 本エンドポイント
//!   への flood が、同じ宛先への正規の AP fetch (actor 取得 / Note fetch) を
//!   巻き添えで枯渇させないため。
//! - media-proxy 自体のサイズ上限・format 検証・timeout。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use crate::media_proxy_route::{self, ProxyQuery};
use crate::state::AppState;

pub async fn handle(State(state): State<AppState>, Query(q): Query<ProxyQuery>) -> Response {
    let parsed = match media_proxy_route::validate(&q, state.allows_private_egress()) {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let Some(host) = parsed.host_str() else {
        return media_proxy_route::error_400("url missing host");
    };
    if !state.try_acquire_media_proxy_fetch(host) {
        return media_proxy_route::error_status(
            StatusCode::TOO_MANY_REQUESTS,
            "media-proxy rate limit exceeded for this host; retry later",
        );
    }
    // ホストローテート flood への第二の bound (per-domain だけでは
    // 総並列度を抑えられない)。permit は fetch 完了まで保持する。
    let Some(_slot) = state.try_acquire_media_proxy_slot() else {
        return media_proxy_route::error_status(
            StatusCode::TOO_MANY_REQUESTS,
            "media-proxy concurrency limit reached; retry later",
        );
    };
    media_proxy_route::fetch_and_respond(&state, parsed.as_str(), &q.variant).await
}
