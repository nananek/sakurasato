//! `GET /api/v1/media/proxy?url=...&variant=...` ── TUI 向けの画像プロキシ。
//!
//! TUI (ホスト端末プロセス) は本サーバの local API 経由でアバター等を取り、
//! さらにその先 (= media-proxy コンテナ) で外部 GET と画像デコードを行う
//! [[Issue #36]]。これで:
//!
//! - TUI ホストプロセスから直接外部 GET が出なくなる
//!   (= ホスト LAN / クラウド IMDS への SSRF リスクを縮小)
//! - server 本体は外部 URL から取ったバイト列を一切デコードしない
//!   (= [`crate::media_proxy_client::MediaProxyClient`] が WebP 再エンコード
//!   済みバイト列を返してくる)
//!
//! コア (URL/variant 検証 + media-proxy 呼び出し + エラーマッピング) は
//! [`crate::media_proxy_route`] に共通化されている ── 公開 (無認証)
//! `GET /media-proxy` ([`crate::routes::media_proxy`], `MiAuth` 経路の画像用)
//! と実装を共有する。本ハンドラは `/api/v1/*` 全体にかかる Bearer 認証
//! ([`super::auth::require_token`]) の内側にいるため、追加のレート制限は
//! 掛けない (= 単一の TUI 利用者のみが Bearer を持つ)。
//!
//! ## レスポンス / クエリパラメータ / キャッシュ
//!
//! [`crate::media_proxy_route`] のモジュールドキュメントを参照。

use axum::extract::{Query, State};
use axum::response::Response;

use crate::media_proxy_route::{ProxyQuery, fetch_and_respond, validate};
use crate::state::AppState;

pub async fn handle(State(state): State<AppState>, Query(q): Query<ProxyQuery>) -> Response {
    let parsed = match validate(&q, state.allows_private_egress()) {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    fetch_and_respond(&state, parsed.as_str(), &q.variant).await
}
