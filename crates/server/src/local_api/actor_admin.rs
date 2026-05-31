//! `POST /api/v1/actor/lock` / `POST /api/v1/actor/unlock` ── Issue #66 / M12。
//!
//! `actor.manually_approves_followers` を local API 経由で切替える。CLI
//! (`sakurasato-server actor lock/unlock`) と同じ挙動 ── 切替時は actor
//! `Update` activity を follower 全員に配信し、相手側 UI の鍵アカ表示を更新
//! させる。
//!
//! pytest 連合テストから UDS + Bearer で叩けるよう、CLI と並列に local API
//! を生やしている。TUI からも将来的にここを呼べる。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tracing::error;

use crate::actor_admin::set_lock_state;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct LockResponse {
    pub ap_id: String,
    pub manually_approves_followers: bool,
    /// この呼び出しで `delivery_queue` に積まれた Update activity の本数。
    /// 既に同じ状態だったときは 0 (= no-op)。
    pub queued_deliveries: usize,
    /// 値が実際に切り替わったか。idempotent re-invoke では false。
    pub changed: bool,
}

pub async fn lock(State(state): State<AppState>) -> Response {
    apply(&state, true).await
}

pub async fn unlock(State(state): State<AppState>) -> Response {
    apply(&state, false).await
}

async fn apply(state: &AppState, next: bool) -> Response {
    match set_lock_state(state, next).await {
        Ok((updated, queued, changed)) => {
            let body = LockResponse {
                ap_id: updated.ap_id,
                manually_approves_followers: updated.manually_approves_followers,
                queued_deliveries: queued,
                changed,
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(err) => {
            error!(?err, next, "POST /api/v1/actor/{{lock,unlock}}: failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
