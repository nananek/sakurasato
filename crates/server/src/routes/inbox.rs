//! Inbox endpoints. M3a only accepts the POST and returns 202 (Accepted)
//! so remote servers learn the inbox exists; verification, parsing, and
//! handler dispatch land in M3b once HTTP signatures are wired.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub async fn shared_inbox() -> Response {
    placeholder()
}

pub async fn user_inbox(Path(_name): Path<String>) -> Response {
    placeholder()
}

fn placeholder() -> Response {
    // 202 Accepted: the activity is queued for processing. Real processing
    // (signature verification, side-effect application) is wired in M3b.
    (
        StatusCode::ACCEPTED,
        "inbox is online but processing is not yet implemented (M3b)",
    )
        .into_response()
}
