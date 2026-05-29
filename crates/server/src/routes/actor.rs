//! `ActivityPub` actor JSON-LD.
//!
//! Returns the local actor for `<name>`. The `private_key_pem` is never
//! exposed — the response only carries the public half via the `publicKey`
//! object that remote servers need for HTTP signature verification.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;

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

    let json = ActorJson {
        context: vec![
            serde_json::Value::String("https://www.w3.org/ns/activitystreams".into()),
            serde_json::Value::String("https://w3id.org/security/v1".into()),
        ],
        actor_type: row.actor_type,
        id: row.ap_id.clone(),
        preferred_username: row.preferred_username,
        name: row.display_name,
        summary: row.summary,
        inbox: row.inbox_url,
        outbox: row.outbox_url,
        followers: row.followers_url,
        following: row.following_url,
        public_key: PublicKey {
            id: row.public_key_id,
            owner: row.ap_id.clone(),
            public_key_pem: row.public_key_pem,
        },
        icon: row.icon_url.map(|url| MediaAttachment {
            media_type: "Image",
            url,
        }),
        image: row.image_url.map(|url| MediaAttachment {
            media_type: "Image",
            url,
        }),
        also_known_as: row.also_known_as.0,
        moved_to: row.moved_to_ap_id,
        endpoints: row
            .shared_inbox_url
            .map(|url| Endpoints { shared_inbox: url }),
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/activity+json"),
    );
    (headers, Json(json)).into_response()
}
