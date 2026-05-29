//! Outbox endpoint. Returns an empty `OrderedCollection` so remote servers
//! can probe the endpoint shape; real paging arrives in M3c with the
//! Create/Note send path.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct OrderedCollection {
    #[serde(rename = "@context")]
    pub context: &'static str,
    #[serde(rename = "type")]
    pub collection_type: &'static str,
    pub id: String,
    #[serde(rename = "totalItems")]
    pub total_items: u64,
    #[serde(rename = "orderedItems")]
    pub ordered_items: Vec<serde_json::Value>,
}

pub async fn user_outbox(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let host = &state.config().server.host;
    let row = match repo::actor::get_by_username_host(state.pool(), &name, host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, "outbox actor lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let body = OrderedCollection {
        context: "https://www.w3.org/ns/activitystreams",
        collection_type: "OrderedCollection",
        id: row
            .outbox_url
            .unwrap_or_else(|| format!("{}/outbox", row.ap_id)),
        total_items: 0,
        ordered_items: vec![],
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/activity+json"),
    );
    (headers, Json(body)).into_response()
}
