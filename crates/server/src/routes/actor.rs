//! `ActivityPub` actor JSON-LD.
//!
//! Returns the local actor for `<name>`. The `private_key_pem` is never
//! exposed — the response only carries the public half via the `publicKey`
//! object that remote servers need for HTTP signature verification.
//!
//! M3b: 公開鍵を二系統で公開する。
//!   - `publicKey` (RSA, SPKI PEM) — cavage HTTP signatures + RSA-SHA256 を
//!     要求する Mastodon 系の互換性のためそのまま。
//!   - `assertionMethod` (FEP-521a Multikey, Ed25519) — RFC 9421 HTTP Message
//!     Signatures や Misskey 系 (Iceshrimp / Sharkey) で Ed25519 を受け付け
//!     る実装向け。Ed25519 鍵を持たない (古い) actor では omit。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;

use crate::multikey;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ActorJson {
    #[serde(rename = "@context")]
    pub context: Vec<serde_json::Value>,
    #[serde(rename = "type")]
    pub actor_type: String,
    pub id: String,
    #[serde(rename = "preferredUsername")]
    pub preferred_username: String,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub inbox: String,
    pub outbox: Option<String>,
    pub followers: Option<String>,
    pub following: Option<String>,
    #[serde(rename = "publicKey")]
    pub public_key: PublicKey,
    /// FEP-521a `assertionMethod`: 追加の公開鍵 (Ed25519 など) を Multikey
    /// 形式で並べる。空のときは omit してフィールドごと消し、レガシー実装
    /// (`assertionMethod` をパースできない) を混乱させないようにする。
    #[serde(rename = "assertionMethod", skip_serializing_if = "Vec::is_empty")]
    pub assertion_method: Vec<Multikey>,
    pub icon: Option<MediaAttachment>,
    pub image: Option<MediaAttachment>,
    #[serde(rename = "alsoKnownAs", skip_serializing_if = "Vec::is_empty")]
    pub also_known_as: Vec<String>,
    #[serde(rename = "movedTo", skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    /// Mastodon / Misskey 互換の鍵アカフラグ (Issue #66 / M12)。
    /// `true` のとき相手側 UI で「フォロー承認制」のバッジが出る。
    /// 後方互換のため `false` でも明示的に出す ── Mastodon 自身も常時 emit。
    #[serde(rename = "manuallyApprovesFollowers")]
    pub manually_approves_followers: bool,
    /// displayName / summary に埋め込まれた `:shortcode:` の `Emoji` tag
    /// (= note 本文側と同じ [`crate::local_api::emoji_tag::build_emoji_tag`]
    /// 形状、byte 一致)。受信側サーバはこれで displayName の絵文字を学習・
    /// 画像化できる。**空のときは omit** (= 絵文字を持たない actor は従来の
    /// wire と完全互換)。
    #[serde(rename = "tag", skip_serializing_if = "Vec::is_empty")]
    pub tag: Vec<serde_json::Value>,
    #[serde(rename = "endpoints", skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Endpoints>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_field_names)] // serde rename needs the explicit pem field name
pub struct PublicKey {
    pub id: String,
    pub owner: String,
    #[serde(rename = "publicKeyPem")]
    pub public_key_pem: String,
}

/// FEP-521a / W3C VC Data Integrity の Multikey 表現。
#[derive(Debug, Serialize)]
pub struct Multikey {
    pub id: String,
    #[serde(rename = "type")]
    pub key_type: &'static str,
    pub controller: String,
    #[serde(rename = "publicKeyMultibase")]
    pub public_key_multibase: String,
}

#[derive(Debug, Serialize)]
pub struct MediaAttachment {
    #[serde(rename = "type")]
    pub media_type: &'static str,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct Endpoints {
    #[serde(rename = "sharedInbox")]
    pub shared_inbox: String,
}

pub async fn actor_json(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let host = &state.config().server.host;
    let row = match repo::actor::get_by_username_host(state.pool(), &name, host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, "actor lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let json = build_actor_json(&row, resolve_actor_emoji_tags(&state, &row).await);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/activity+json"),
    );
    (headers, Json(json)).into_response()
}

/// `ActorRow` から `ActivityPub` actor JSON を組み立てる。
///
/// M3a 以来 `actor_json` ハンドラ専用だったが、M7 で **Update Activity** の
/// `object` フィールドにも actor JSON を載せる必要が出たので、ハンドラ外
/// から再利用できるように切り出した。
///
/// `row.private_key_pem` は **常に** レスポンスに含めない (公開鍵側だけ載せる)。
/// 呼び出し側は `ActorJson` → `serde_json::to_value(..)` で `object` に
/// 埋め込める。
///
/// `tags` は解決済みの AP `Emoji` tag 配列 (= [`resolve_actor_emoji_tags`]
/// の結果)。本関数は純関数のまま、I/O を呼び出し側に残す。
pub fn build_actor_json(
    row: &sakurasato_core::model::ActorRow,
    tags: Vec<serde_json::Value>,
) -> ActorJson {
    // Ed25519 鍵が登録されていれば assertionMethod に Multikey として並べる。
    // PEM パース失敗は 500 にせず、警告ログを残して omit する: RSA だけでも
    // 連合は機能するし、500 で actor 取得が永続的に壊れるよりはマシ。
    let mut assertion_method = Vec::new();
    if let (Some(ed_id), Some(ed_pem)) = (
        row.ed25519_public_key_id.as_ref(),
        row.ed25519_public_key_pem.as_ref(),
    ) {
        match multikey::ed25519_pem_to_multibase(ed_pem) {
            Ok(mb) => assertion_method.push(Multikey {
                id: ed_id.clone(),
                key_type: "Multikey",
                controller: row.ap_id.clone(),
                public_key_multibase: mb,
            }),
            Err(err) => tracing::warn!(
                ?err,
                ap_id = %row.ap_id,
                "skipping Ed25519 assertionMethod: failed to encode multibase",
            ),
        }
    }

    // @context: AS2 + 旧 security/v1 (publicKey) は常に。Multikey を載せると
    // きは multikey context も追加する。古い実装が知らない URI を含むと拒否
    // するケースは見当たらないが、不要なら載せないでおく。
    //
    // `manuallyApprovesFollowers` は AS2 vocab に元から無い拡張語。Mastodon
    // / Pleroma / Misskey は context に `as:manuallyApprovesFollowers` の
    // alias を明示的に並べる慣習があるので、それに従う。受信側が strict な
    // JSON-LD processor を回したときに `manuallyApprovesFollowers` 用語が
    // 解決されなくて取りこぼされる事故を防ぐ (Issue #66 / M12)。
    let mut context = vec![
        serde_json::Value::String("https://www.w3.org/ns/activitystreams".into()),
        serde_json::Value::String("https://w3id.org/security/v1".into()),
    ];
    if !assertion_method.is_empty() {
        context.push(serde_json::Value::String(
            "https://w3id.org/security/multikey/v1".into(),
        ));
    }
    context.push(serde_json::json!({
        "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
    }));

    ActorJson {
        context,
        actor_type: row.actor_type.clone(),
        id: row.ap_id.clone(),
        preferred_username: row.preferred_username.clone(),
        name: row.display_name.clone(),
        summary: row.summary.clone(),
        inbox: row.inbox_url.clone(),
        outbox: row.outbox_url.clone(),
        followers: row.followers_url.clone(),
        following: row.following_url.clone(),
        public_key: PublicKey {
            id: row.public_key_id.clone(),
            owner: row.ap_id.clone(),
            public_key_pem: row.public_key_pem.clone(),
        },
        assertion_method,
        icon: row.icon_url.clone().map(|url| MediaAttachment {
            media_type: "Image",
            url,
        }),
        image: row.image_url.clone().map(|url| MediaAttachment {
            media_type: "Image",
            url,
        }),
        also_known_as: row.also_known_as.0.clone(),
        moved_to: row.moved_to_ap_id.clone(),
        manually_approves_followers: row.manually_approves_followers,
        tag: tags,
        endpoints: row
            .shared_inbox_url
            .clone()
            .map(|url| Endpoints { shared_inbox: url }),
    }
}

/// ローカル actor の `display_name` / `summary` に含まれる `:shortcode:` を
/// DB の emoji 行に解決して AP `Emoji` tag 配列を組み立てる。
///
/// note 本文側 [`crate::local_api::notes::resolve_emoji_tags`] と同じ
/// [`crate::local_api::notes::parse_emoji_shortcodes`] +
/// [`crate::local_api::emoji_tag::build_emoji_tag`] を共有するため、連合 wire
/// 上の `Emoji` tag は note と byte 一致する (= 受信側は `tag` さえあれば
/// displayName の絵文字を学習できる、バグ 1 の actor 側修正)。shortcode 抽出
/// は [`crate::miauth::conv::collect_actor_emoji_shortcodes`] を使う
/// (= `MiAuth` ユーザー `emojis` map と同一規約)。
///
/// - 解決できない shortcode は黙って drop (note 側と同じ fail-open)。
/// - 同一 shortcode に local / learned-remote 両行がある場合は **local 優先**
///   (remote 行は local 行で解決済みなら捨てる)。
pub(crate) async fn resolve_actor_emoji_tags(
    state: &AppState,
    actor: &sakurasato_core::model::ActorRow,
) -> Vec<serde_json::Value> {
    let shortcodes = crate::miauth::conv::collect_actor_emoji_shortcodes(actor);
    if shortcodes.is_empty() {
        return Vec::new();
    }
    let Ok(rows) = repo::emoji::list_by_shortcodes(state.pool(), &shortcodes).await else {
        return Vec::new();
    };
    // local 優先で shortcode → row に畳む (learned-remote 行は local 行に
    // 上書きされる ── `BTreeMap` なので shortcode 昇順 = 決定的な tag 順)。
    let mut by_shortcode: std::collections::BTreeMap<String, sakurasato_core::model::EmojiRow> =
        std::collections::BTreeMap::new();
    for row in rows {
        if row.host.is_some() && by_shortcode.contains_key(&row.shortcode) {
            continue;
        }
        by_shortcode.insert(row.shortcode.clone(), row);
    }
    let host = &state.config().server.host;
    let mut out = Vec::with_capacity(by_shortcode.len());
    for (_, row) in by_shortcode {
        if let Some(tag) = crate::local_api::emoji_tag::build_emoji_tag(host, &row) {
            out.push(tag);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sakurasato_core::model::ActorRow;
    use serde_json::json;
    use sqlx::types::Json as SqlxJson;

    fn fake_actor() -> ActorRow {
        ActorRow {
            id: 42,
            ap_id: "https://sakurasato.test/users/me".into(),
            preferred_username: "me".into(),
            host: "sakurasato.test".into(),
            display_name: Some("Alice".into()),
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: "https://sakurasato.test/users/me/inbox".into(),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: "k".into(),
            public_key_pem: "p".into(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: SqlxJson(vec![]),
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".into(),
            manually_approves_followers: false,
            birthday: None,
            location: None,
            lang: None,
            followed_message: None,
            fields: SqlxJson(vec![]),
            followers_count: 0,
            following_count: 0,
            notes_count: 0,
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// 絵文字を解決できなかった (= tags 空) ときは `tag` key 自体を omit ──
    /// 絵文字を持たない actor は従来の wire と完全互換。
    #[test]
    fn build_actor_json_omits_tag_when_empty() {
        let json = serde_json::to_value(build_actor_json(&fake_actor(), Vec::new())).unwrap();
        assert!(
            json.get("tag").is_none(),
            "empty tags must omit the `tag` key entirely"
        );
    }

    /// 解決済み `Emoji` tag は `tag: [...]` としてそのまま載る (note 側の
    /// [`crate::local_api::emoji_tag::build_emoji_tag`] と同じ形状)。
    #[test]
    fn build_actor_json_emits_resolved_emoji_tags() {
        let tag = json!({
            "type": "Emoji",
            "id": "https://sakurasato.test/emojis/sakura",
            "name": ":sakura:",
            "updated": "2026-08-12T00:00:00Z",
            "icon": {
                "type": "Image",
                "mediaType": "image/webp",
                "url": "https://sakurasato.test/media/emoji/local/sakura.webp",
            },
        });
        let json =
            serde_json::to_value(build_actor_json(&fake_actor(), vec![tag.clone()])).unwrap();
        assert_eq!(json["tag"], serde_json::json!([tag]));
    }
}
