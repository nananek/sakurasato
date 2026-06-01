//! `GET /api/v1/emojis?prefix=...&limit=N` ── ローカル絵文字の shortcode
//! prefix 検索 (Issue #101)。
//!
//! TUI が compose / reaction prompt 入力中に `:foo` まで打った段階で叩き、
//! popup overlay で候補を表示する。本 PR ではローカル絵文字のみを返し、
//! リモート絵文字 (`:foo@host:`) は対象外 (= 別 issue)。
//!
//! 認証は他 `/api/v1/*` と同じ Bearer (UDS 上で middleware が処理)。
//! 結果は **画像 URL + `media_type`** を返すので、TUI 側は `:foo:` 挿入時に
//! 表示用 URL を別途取得し直す必要がない。

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::local_api::media::build_media_url;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// 前方一致検索キー (ASCII-lowercase 比較)。空文字 / 未指定なら全件
    /// (= `limit` まで)。
    #[serde(default)]
    pub prefix: Option<String>,
    /// 1..=100 にクランプ。未指定なら 20。
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EmojiItem {
    pub shortcode: String,
    pub url: String,
    pub media_type: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<EmojiItem>,
}

pub async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let prefix = q.prefix.as_deref().unwrap_or("").trim();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows = match repo::emoji::list_local_by_prefix(state.pool(), prefix, limit).await {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, "list emojis failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let host = &state.config().server.host;
    let items: Vec<EmojiItem> = rows
        .into_iter()
        .map(|row| EmojiItem {
            url: build_media_url(host, &row.image_key),
            shortcode: row.shortcode,
            media_type: row.media_type,
            category: row.category,
            aliases: row.aliases.0,
        })
        .collect();

    (StatusCode::OK, Json(ListResponse { items })).into_response()
}
