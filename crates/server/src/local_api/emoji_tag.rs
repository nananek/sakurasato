//! 連合 outbound 用の AP `Emoji` tag builder。note 本文 ([`super::notes`]) と
//! reaction ([`super::reactions`]) で共有し、2 つの emit 箇所が drift しないように
//! する。
//!
//! FEP-9098 の必須/標準フィールド (`type` / `id` / `name` / `updated` / `icon`) に
//! 加えて、受信側が読めれば使える **additive 拡張**を omit-when-empty で載せる
//! ── これで他サーバ (Nekonoverse / `CherryPick` 等) が連合経由でも category /
//! aliases / license / sensitive を拾える。AP wire 上 `category` / `aliases` /
//! `sensitive` は FEP-9098 標準外だが additive なので、理解しない実装 (Misskey /
//! Mastodon) は無視するだけで害は無い。`_misskey_license` のみ Misskey が公式に
//! 定義する `Emoji` 拡張 (misskey-hub.net/ns)。

use sakurasato_core::model::EmojiRow;
use serde_json::{Value, json};

use crate::local_api::media::build_media_url;

/// ローカル emoji row → AP `Emoji` tag。`image_key` が `None` のときは `None`
/// (= 画像 URL を出せないので tag 自体を出さない = テキストフォールバック)。
///
/// `host` は公開 AP host。`id` / `name` / `icon` は dereferenceable な
/// [`crate::routes::emoji`] object と byte 一致する。
pub(crate) fn build_emoji_tag(host: &str, row: &EmojiRow) -> Option<Value> {
    let image_key = row.image_key.as_deref()?;
    let mut tag = json!({
        "type": "Emoji",
        "id": format!("https://{host}/emojis/{}", row.shortcode),
        "name": format!(":{}:", row.shortcode),
        "updated": row.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "icon": {
            "type": "Image",
            "mediaType": row.media_type,
            "url": build_media_url(host, image_key),
        },
    });
    let obj = tag.as_object_mut().expect("json! built an object");
    // category: Some のときだけ。
    if let Some(category) = &row.category {
        obj.insert("category".into(), Value::String(category.clone()));
    }
    // aliases / keywords: 非空のときだけ。両方出す ── Nekonoverse がどちらの key を
    // 読むか実装差があるため belt-and-suspenders (Misskey/Mastodon は無視)。
    if !row.aliases.0.is_empty() {
        let arr = Value::Array(row.aliases.0.iter().cloned().map(Value::String).collect());
        obj.insert("aliases".into(), arr.clone());
        obj.insert("keywords".into(), arr);
    }
    // sensitive: true のときだけ。
    if row.is_sensitive {
        obj.insert("sensitive".into(), Value::Bool(true));
    }
    // _misskey_license: license が Some のときだけ ({freeText: null} は出さない)。
    if let Some(license) = &row.license {
        obj.insert("_misskey_license".into(), json!({ "freeText": license }));
    }
    Some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::types::Json as SqlxJson;

    fn row(
        image_key: Option<&str>,
        category: Option<&str>,
        aliases: &[&str],
        is_sensitive: bool,
        license: Option<&str>,
    ) -> EmojiRow {
        EmojiRow {
            id: 1,
            shortcode: "blobcat".into(),
            host: None,
            category: category.map(str::to_string),
            aliases: SqlxJson(aliases.iter().map(|s| (*s).to_string()).collect()),
            image_key: image_key.map(str::to_string),
            media_type: "image/webp".into(),
            ap_id: None,
            is_local: true,
            license: license.map(str::to_string),
            is_sensitive,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_failed_at: None,
        }
    }

    #[test]
    fn full_metadata_is_emitted_additively() {
        let r = row(
            Some("emoji/local/blobcat.webp"),
            Some("blob"),
            &["cat", "neko"],
            true,
            Some("CC-BY-4.0"),
        );
        let tag = build_emoji_tag("sakurasato.test", &r).expect("image_key present");
        // 標準フィールドは保持。
        assert_eq!(tag["type"], "Emoji");
        assert_eq!(tag["id"], "https://sakurasato.test/emojis/blobcat");
        assert_eq!(tag["name"], ":blobcat:");
        assert_eq!(
            tag["icon"]["url"],
            "https://sakurasato.test/media/emoji/local/blobcat.webp"
        );
        // 拡張フィールド。
        assert_eq!(tag["category"], "blob");
        assert_eq!(tag["aliases"], json!(["cat", "neko"]));
        assert_eq!(tag["keywords"], json!(["cat", "neko"]));
        assert_eq!(tag["sensitive"], true);
        assert_eq!(tag["_misskey_license"]["freeText"], "CC-BY-4.0");
    }

    #[test]
    fn minimal_row_is_byte_identical_to_pre_enrichment() {
        // category None / aliases 空 / sensitive false / license None →
        // 拡張 key は一切出ない (= 既存挙動の additivity 回帰ガード)。
        let r = row(Some("emoji/local/blobcat.webp"), None, &[], false, None);
        let tag = build_emoji_tag("sakurasato.test", &r).expect("image_key present");
        let obj = tag.as_object().unwrap();
        // map の順序は serde_json の feature 次第なので set で比較する。
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["icon", "id", "name", "type", "updated"]);
        // 拡張 key が一切無いこと (= 既存挙動の additivity 回帰ガード)。
        for k in [
            "category",
            "aliases",
            "keywords",
            "sensitive",
            "_misskey_license",
        ] {
            assert!(!obj.contains_key(k), "enrichment key {k} must be absent");
        }
    }

    #[test]
    fn none_when_image_key_missing() {
        let r = row(None, Some("blob"), &["cat"], true, Some("x"));
        assert!(build_emoji_tag("sakurasato.test", &r).is_none());
    }
}
