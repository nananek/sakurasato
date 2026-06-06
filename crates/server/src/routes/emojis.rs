//! 公開 AP host 上の **絵文字 discovery エンドポイント** ── 他サーバが我々の
//! ローカル絵文字メタデータを import するときに参照する REST。
//!
//! - `GET /api/v1/custom_emojis` ── Mastodon 互換 `CustomEmoji[]` (bare array)。
//!   Nekonoverse の REST fallback (inline `Emoji` tag 無し reaction を受けたとき
//!   `GET /api/v1/custom_emojis` を `shortcode` で線形探索する) と、Mastodon 系
//!   ツールの picker / bulk-import がこれを叩く。
//! - `GET`/`POST /api/emojis` ── Misskey 互換 `{ emojis: EmojiSimple[] }`。Misskey 系
//!   discovery 用。MiAuth listener ([`crate::miauth::emojis`]) の同名 endpoint とは
//!   **別物** (あちらは Tailscale 限定で公開連合からは届かない)。
//!
//! いずれも **無認証・ローカル絵文字のみ** (`host IS NULL`)。`image_key` が `None`
//! の row は公開 URL を作れないので除外する (Issue #135)。
//!
//! clean-room: 一次資料は Mastodon `GET /api/v1/custom_emojis` の公開 doc と
//! Misskey `EmojiSimple` schema (api-doc.misskey.io / `misskey_dart` MIT) のみ。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;

use crate::local_api::media::build_media_url;
use crate::state::AppState;

/// 1 度に列挙する最大件数 ([`crate::miauth::emojis`] の `EMOJIS_FETCH_LIMIT` と同値)。
const FETCH_LIMIT: i64 = 10_000;

/// Mastodon `CustomEmoji` (+ `aliases` 拡張)。**`snake_case` wire** なので
/// `rename_all` は付けない (Mastodon の `custom_emojis` は `static_url` /
/// `visible_in_picker` のような `snake_case`)。
#[derive(Debug, Serialize)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
    /// 静的 (非アニメ) 版。我々は 1 emoji = 単一 WebP しか持たないので `url` と同値。
    /// Mastodon client は必須扱いで dereference するため常に出す。
    pub static_url: String,
    pub visible_in_picker: bool,
    /// Mastodon の `category` は optional ── `None` のときは key 自体を省く。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Mastodon 非標準の拡張。Mastodon は未知 field を無視し、Nekonoverse は
    /// `aliases` を読むので additive に載せる。空でも `[]` で常に出す。
    pub aliases: Vec<String>,
}

/// `GET /api/v1/custom_emojis` ── Mastodon 互換 (bare array)。
pub async fn custom_emojis(State(state): State<AppState>) -> Response {
    let rows = match repo::emoji::list_local_by_prefix(state.pool(), "", FETCH_LIMIT).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                "GET /api/v1/custom_emojis: list_local_by_prefix failed"
            );
            return (StatusCode::SERVICE_UNAVAILABLE, "emoji listing unavailable").into_response();
        }
    };
    let host = &state.config().server.host;
    let items: Vec<CustomEmoji> = rows
        .into_iter()
        .filter_map(|row| {
            // Issue #135: image_key が無い row は公開 URL を作れない → 除外。
            let image_key = row.image_key.as_deref()?;
            let url = build_media_url(host, image_key);
            Some(CustomEmoji {
                shortcode: row.shortcode,
                static_url: url.clone(),
                url,
                visible_in_picker: true,
                category: row.category,
                aliases: row.aliases.0,
            })
        })
        .collect();
    Json(items).into_response()
}

/// Misskey `EmojiSimple`。`isSensitive` / `localOnly` は camelCase wire。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEmojiSimple {
    pub aliases: Vec<String>,
    pub name: String,
    /// Misskey は `category` を nullable で **常に出す** (`None` → `null`、省略しない)。
    pub category: Option<String>,
    pub url: String,
    /// 現状 emoji テーブルに sensitive 列が無いので一律 `false` (PR2 で実値化予定)。
    pub is_sensitive: bool,
    pub local_only: bool,
}

#[derive(Debug, Serialize)]
pub struct EmojisResponse {
    pub emojis: Vec<PublicEmojiSimple>,
}

/// `GET`/`POST /api/emojis` ── Misskey 互換 discovery。Misskey は GET/POST 両対応
/// なので両方受ける (body は無視 = anonymous-public)。
pub async fn misskey_emojis(State(state): State<AppState>) -> Response {
    let rows = match repo::emoji::list_local_by_prefix(state.pool(), "", FETCH_LIMIT).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "GET /api/emojis: list_local_by_prefix failed");
            return (StatusCode::SERVICE_UNAVAILABLE, "emoji listing unavailable").into_response();
        }
    };
    let host = &state.config().server.host;
    let emojis: Vec<PublicEmojiSimple> = rows
        .into_iter()
        .filter_map(|row| {
            let image_key = row.image_key.as_deref()?;
            Some(PublicEmojiSimple {
                aliases: row.aliases.0,
                name: row.shortcode,
                category: row.category,
                url: build_media_url(host, image_key),
                is_sensitive: false,
                local_only: false,
            })
        })
        .collect();
    Json(EmojisResponse { emojis }).into_response()
}
