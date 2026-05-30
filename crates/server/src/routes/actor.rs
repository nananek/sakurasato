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

    let json = build_actor_json(&row);
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
pub fn build_actor_json(row: &sakurasato_core::model::ActorRow) -> ActorJson {
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
    let mut context = vec![
        serde_json::Value::String("https://www.w3.org/ns/activitystreams".into()),
        serde_json::Value::String("https://w3id.org/security/v1".into()),
    ];
    if !assertion_method.is_empty() {
        context.push(serde_json::Value::String(
            "https://w3id.org/security/multikey/v1".into(),
        ));
    }

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
        endpoints: row
            .shared_inbox_url
            .clone()
            .map(|url| Endpoints { shared_inbox: url }),
    }
}
