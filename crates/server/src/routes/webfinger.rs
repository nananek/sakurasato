//! `RFC 7033` `WebFinger`.
//!
//! Only `acct:<user>@<host>` resources are answered, and only when `<host>`
//! matches the configured `server.host` (i.e. requests for *our* user from
//! someone else's server). Anything else returns 404 — we are a single-user
//! instance and have no other accounts to expose.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WebfingerQuery {
    pub resource: String,
}

#[derive(Debug, Serialize)]
pub struct WebfingerResponse {
    pub subject: String,
    pub links: Vec<WebfingerLink>,
}

#[derive(Debug, Serialize)]
pub struct WebfingerLink {
    pub rel: String,
    #[serde(rename = "type")]
    pub link_type: String,
    pub href: String,
}

pub async fn handle(
    State(state): State<AppState>,
    Query(query): Query<WebfingerQuery>,
) -> Response {
    let Some((username, host)) = parse_acct(&query.resource) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if host != state.config().server.host {
        return StatusCode::NOT_FOUND.into_response();
    }
    let row = match repo::actor::get_by_username_host(state.pool(), username, host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, "webfinger lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let resp = WebfingerResponse {
        subject: format!("acct:{}@{}", row.preferred_username, row.host),
        links: vec![WebfingerLink {
            rel: "self".into(),
            link_type: "application/activity+json".into(),
            href: row.ap_id,
        }],
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/jrd+json"),
    );
    (headers, Json(resp)).into_response()
}

/// Split `acct:user@host` into `(user, host)`. Returns `None` for any other
/// scheme or malformed resource.
fn parse_acct(resource: &str) -> Option<(&str, &str)> {
    let body = resource.strip_prefix("acct:")?;
    let (user, host) = body.split_once('@')?;
    if user.is_empty() || host.is_empty() {
        return None;
    }
    Some((user, host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_acct_happy_path() {
        assert_eq!(
            parse_acct("acct:alice@example.com"),
            Some(("alice", "example.com"))
        );
    }

    #[test]
    fn parse_acct_rejects_unknown_scheme() {
        assert!(parse_acct("https://example.com/users/alice").is_none());
        assert!(parse_acct("mailto:alice@example.com").is_none());
    }

    #[test]
    fn parse_acct_rejects_empty_parts() {
        assert!(parse_acct("acct:@example.com").is_none());
        assert!(parse_acct("acct:alice@").is_none());
        assert!(parse_acct("acct:").is_none());
    }
}
