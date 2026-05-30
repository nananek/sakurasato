//! Axum routing. M3a wires `WebFinger` / nodeinfo / actor JSON; inbox /
//! outbox are placeholders that the M3b PR fills in once HTTP signature
//! verification is online.

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod actor;
pub mod inbox;
pub mod media;
pub mod nodeinfo;
pub mod outbox;
pub mod permalink;
pub mod webfinger;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger::handle))
        .route("/.well-known/nodeinfo", get(nodeinfo::well_known))
        .route("/nodeinfo/2.1", get(nodeinfo::v2_1))
        .route("/users/{name}", get(actor::actor_json))
        .route("/users/{name}/inbox", post(inbox::user_inbox))
        .route("/users/{name}/outbox", get(outbox::user_outbox))
        .route("/inbox", post(inbox::shared_inbox))
        // M4 PR1 — 最小 Web。permalink は AP JSON 兼用化を M4 PR2 で行う。
        .route("/notes/{id}", get(permalink::handle))
        // `{*key}` で `/media/path/to/object.png` のスラッシュ入りキーを 1 つの
        // `String` にキャプチャする (axum 0.8 ワイルドカード)。
        .route("/media/{*key}", get(media::handle))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
