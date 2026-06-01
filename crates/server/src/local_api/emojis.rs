//! `GET /api/v1/emojis?prefix=...&q=...&limit=N` ── ローカル絵文字の検索
//! (Issue #101 + Issue #130)。
//!
//! TUI が compose / reaction prompt 入力中に `:foo` まで打った段階で叩き、
//! popup overlay で候補を表示する。本 PR ではローカル絵文字のみを返し、
//! リモート絵文字 (`:foo@host:`) は対象外 (= 別 issue)。
//!
//! 認証は他 `/api/v1/*` と同じ Bearer (UDS 上で middleware が処理)。
//! 結果は **画像 URL + `media_type`** を返すので、TUI 側は `:foo:` 挿入時に
//! 表示用 URL を別途取得し直す必要がない。
//!
//! ## 検索モード
//!
//! - `q=foo` を渡すと **`shortcode` / `aliases` の部分一致** (Issue #130)。
//!   将来 keystroke fetch (server 問い合わせ) を入れたときの布石。
//! - `prefix=foo` または無指定なら従来どおり **shortcode 前方一致**。
//! - 両方与えられた場合は `q` が優先。
//!
//! ## limit と帯域
//!
//! お一人様サーバの UDS 経由なので帯域コストは無視できる。`MAX_LIMIT = 10000`
//! 件を一気に返しても client 側 substring マッチはサブ ms。`DEFAULT_LIMIT = 20`
//! は明示的に多件取りに来ない経路 (= 旧版クライアントや CLI など) を意識した
//! 保守的な既定。

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
/// 明示要求の上限。お一人様 + UDS 前提なので 10000 件返しても帯域問題は出ず、
/// TUI 側で全件キャッシュ → client-side substring 検索する設計と整合する
/// (Issue #130)。
const MAX_LIMIT: i64 = 10000;

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// 前方一致検索キー (ASCII-lowercase 比較)。空文字 / 未指定なら全件
    /// (= `limit` まで)。`q` が指定されたときは無視される。
    #[serde(default)]
    pub prefix: Option<String>,
    /// 部分一致検索キー (Issue #130, `shortcode` / `aliases` を `ILIKE` で
    /// 検索)。空文字 / 未指定なら従来挙動 (= `prefix` 経路) を使う。
    #[serde(default)]
    pub q: Option<String>,
    /// `1..=MAX_LIMIT` にクランプ。未指定なら `DEFAULT_LIMIT`。
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `kind` 識別子。現状サーバは `"custom"` (ローカル import 済み画像 emoji)
/// のみ返す。`"unicode"` は TUI 側が `sakurasato_core::unicode_emoji` 由来で
/// クライアント内マージする際に発行するため、サーバが直接返すことは無いが
/// 型としては将来余地として残しておく (= 別経路で server 経由になっても
/// JSON shape を変えずに済む)。
#[derive(Debug, Serialize)]
pub struct EmojiItem {
    /// `"custom"` か `"unicode"`。古いクライアントは `kind` を見ない前提で
    /// もそのまま動く ── custom emoji は `url` / `media_type` が必須で揃って
    /// いる従来形と互換。
    pub kind: &'static str,
    pub shortcode: String,
    /// custom emoji は media 配信 URL、unicode は空文字 (= TUI 側で画像表示
    /// せず文字描画する判定に使う)。`Option` ではなく空文字としているのは
    /// 既存クライアント (`url: String`) の互換性を壊さないため。
    pub url: String,
    pub media_type: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<EmojiItem>,
}

pub async fn list(State(state): State<AppState>, Query(params): Query<ListQuery>) -> Response {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let substring_q = params.q.as_deref().map_or("", str::trim);

    let rows = if substring_q.is_empty() {
        let prefix = params.prefix.as_deref().unwrap_or("").trim();
        repo::emoji::list_local_by_prefix(state.pool(), prefix, limit).await
    } else {
        repo::emoji::search_local_by_substring(state.pool(), substring_q, limit).await
    };
    let rows = match rows {
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
            kind: "custom",
            url: build_media_url(host, &row.image_key),
            shortcode: row.shortcode,
            media_type: row.media_type,
            category: row.category,
            aliases: row.aliases.0,
        })
        .collect();

    (StatusCode::OK, Json(ListResponse { items })).into_response()
}
