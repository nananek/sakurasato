//! `GET /emojis/{shortcode}` ── 我々が note / reaction の inline tag で発行している
//! `Emoji.id` (`https://{host}/emojis/{shortcode}`) を dereferenceable にする。
//! FEP-9098 の `Emoji` object を `application/activity+json` で返す。
//!
//! ローカル絵文字のみ (`host IS NULL`) + `image_key` 必須。未知 / remote /
//! image 無しは 404。FEP-9098 上 `id` の dereference は必須ではないが、解決できる
//! ようにしておくと厳格な JSON-LD 実装との相互運用が増える。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::SecondsFormat;
use sakurasato_core::model::EmojiRow;
use sakurasato_core::repo;
use serde_json::{Value, json};

use crate::local_api::media::build_media_url;
use crate::state::AppState;

/// AP `Emoji` object の `@context` ── AS2 + `toot:Emoji` term alias。
/// actor JSON ([`super::actor`]) の context `Vec` と同じ流儀。
fn emoji_object_context() -> Vec<Value> {
    vec![
        Value::String("https://www.w3.org/ns/activitystreams".to_string()),
        json!({
            "toot": "http://joinmastodon.org/ns#",
            "Emoji": "toot:Emoji",
        }),
    ]
}

/// ローカル emoji row → FEP-9098 `Emoji` object。`image_key` が `None` なら `None`
/// (= dereference させない)。`host` は公開 AP host。inline tag emitter
/// ([`crate::local_api::notes`] / [`crate::local_api::reactions`]) の `id` / `name`
/// / `icon` と byte 一致させる。
pub(crate) fn build_emoji_object(host: &str, row: &EmojiRow) -> Option<Value> {
    let image_key = row.image_key.as_deref()?;
    Some(json!({
        "@context": emoji_object_context(),
        "id": format!("https://{host}/emojis/{}", row.shortcode),
        "type": "Emoji",
        "name": format!(":{}:", row.shortcode),
        "updated": row.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        "icon": {
            "type": "Image",
            "mediaType": row.media_type,
            "url": build_media_url(host, image_key),
        },
    }))
}

/// `GET /emojis/{shortcode}` handler。
pub async fn handle(State(state): State<AppState>, Path(shortcode): Path<String>) -> Response {
    // `get_local_by_shortcode` は `host IS NULL` で絞るので remote は自動的に 404。
    let row = match repo::emoji::get_local_by_shortcode(state.pool(), &shortcode).await {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, shortcode, "GET /emojis/{{shortcode}}: lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let host = &state.config().server.host;
    let Some(obj) = build_emoji_object(host, &row) else {
        // image_key が無い (= まだ焼けていない) emoji は dereference させない。
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/activity+json"),
    );
    (headers, Json(obj)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::types::Json as SqlxJson;

    fn local_emoji_row(image_key: Option<&str>) -> EmojiRow {
        EmojiRow {
            id: 1,
            shortcode: "blobcat".into(),
            host: None,
            category: Some("blob".into()),
            aliases: SqlxJson(vec!["cat".into()]),
            image_key: image_key.map(str::to_string),
            media_type: "image/webp".into(),
            ap_id: None,
            is_local: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_failed_at: None,
        }
    }

    #[test]
    fn build_emoji_object_shape() {
        let row = local_emoji_row(Some("emoji/local/blobcat.webp"));
        let obj = build_emoji_object("sakurasato.test", &row).expect("image_key present");
        assert_eq!(obj["type"], "Emoji");
        assert_eq!(obj["id"], "https://sakurasato.test/emojis/blobcat");
        assert_eq!(obj["name"], ":blobcat:");
        assert_eq!(obj["icon"]["type"], "Image");
        assert_eq!(obj["icon"]["mediaType"], "image/webp");
        assert_eq!(
            obj["icon"]["url"],
            "https://sakurasato.test/media/emoji/local/blobcat.webp"
        );
        assert!(obj["updated"].is_string());
        // @context は AS2 string + toot:Emoji term alias。
        assert_eq!(obj["@context"][0], "https://www.w3.org/ns/activitystreams");
        assert_eq!(obj["@context"][1]["Emoji"], "toot:Emoji");
    }

    #[test]
    fn build_emoji_object_none_when_image_key_null() {
        let row = local_emoji_row(None);
        assert!(build_emoji_object("sakurasato.test", &row).is_none());
    }
}
