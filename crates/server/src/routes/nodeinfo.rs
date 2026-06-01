//! `NodeInfo` 2.1 discovery + payload.
//!
//! See <http://nodeinfo.diaspora.software/>.
//!
//! - `local_posts` は `is_local = TRUE` の Note を DB から集計する。
//! - `active_*` はお一人様サーバ前提で「`local_posts` > 0 ? 1 : 0」(= 投稿が
//!   1 件でもあれば actor は active 扱い)。
//! - `metadata` は `config.server.info` (= `ServerInfo`) を反映する。空なら
//!   `{}` で従来挙動を保つ。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;
use tracing::warn;

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

pub async fn v2_1(State(state): State<AppState>) -> Response {
    // local_posts は DB から動的に集計する。失敗時は 0 にフォールバック
    // (= NodeInfo は読み取り API で全体障害を引き起こすほどクリティカルでない、
    // warn ログだけ残す)。`u64` キャストは件数なので非負。
    let local_posts: u64 = match repo::note::count_local(state.pool()).await {
        Ok(n) => u64::try_from(n).unwrap_or(0),
        Err(err) => {
            warn!(?err, "nodeinfo: count_local failed; returning 0");
            0
        }
    };
    // お一人様サーバなので投稿が 1 件でもあれば active 扱い。
    let active = u64::from(local_posts > 0);

    let metadata = serde_json::to_value(&state.config().server.info).unwrap_or_else(|err| {
        warn!(
            ?err,
            "nodeinfo: failed to serialize server.info; emitting {{}}"
        );
        serde_json::json!({})
    });

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
            users: UsersUsage {
                total: 1,
                active_month: active,
                active_halfyear: active,
            },
            local_posts,
        },
        metadata,
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
