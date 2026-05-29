//! `NodeInfo` 2.1 discovery + payload.
//!
//! See <http://nodeinfo.diaspora.software/>. M3a returns static counts (one
//! local user, zero posts); M3c refreshes the post / active-user counters
//! from the DB once write paths exist.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::state::AppState;

const SCHEMA_2_1: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";

#[derive(Debug, Serialize)]
pub struct Discovery {
    pub links: Vec<DiscoveryLink>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryLink {
    pub rel: String,
    pub href: String,
}

pub async fn well_known(State(state): State<AppState>) -> Response {
    let href = format!("https://{}/nodeinfo/2.1", state.config().server.host);
    let body = Discovery {
        links: vec![DiscoveryLink {
            rel: SCHEMA_2_1.into(),
            href,
        }],
    };
    Json(body).into_response()
}

#[derive(Debug, Serialize)]
pub struct NodeInfo {
    pub version: &'static str,
    pub software: Software,
    pub protocols: Vec<&'static str>,
    pub services: Services,
    #[serde(rename = "openRegistrations")]
    pub open_registrations: bool,
    pub usage: Usage,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Software {
    pub name: &'static str,
    pub version: &'static str,
    pub repository: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Services {
    pub inbound: Vec<&'static str>,
    pub outbound: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub users: UsersUsage,
    #[serde(rename = "localPosts")]
    pub local_posts: u64,
}

#[derive(Debug, Serialize)]
pub struct UsersUsage {
    pub total: u64,
    #[serde(rename = "activeMonth")]
    pub active_month: u64,
    #[serde(rename = "activeHalfyear")]
    pub active_halfyear: u64,
}

pub async fn v2_1(State(_state): State<AppState>) -> Response {
    let body = NodeInfo {
        version: "2.1",
        software: Software {
            name: "sakurasato",
            version: env!("CARGO_PKG_VERSION"),
            repository: "https://github.com/nananek/sakurasato",
        },
        protocols: vec!["activitypub"],
        services: Services {
            inbound: vec![],
            outbound: vec![],
        },
        open_registrations: false,
        usage: Usage {
            // お一人様サーバ。posts は M3c で DB から拾うまで 0。
            users: UsersUsage {
                total: 1,
                active_month: 1,
                active_halfyear: 1,
            },
            local_posts: 0,
        },
        metadata: serde_json::json!({}),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(
            "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"",
        ),
    );
    (headers, Json(body)).into_response()
}
