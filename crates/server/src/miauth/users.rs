//! `POST /api/users/show` (= M14 #159, 親 issue #150)。
//!
//! Misskey 互換クライアント (Milktea / `MissRirica` 等) が「ユーザを開く」
//! 経路。`userId` 直接指定と `username` + `host` 指定の 2 経路をサポートする。
//!
//! ## clean-room
//!
//! - <https://api-doc.misskey.io/api/endpoints/users/show>
//!
//! observed wire shape (= `misskey-py` `users_show()` で本物 Misskey の admin を
//! 引いたときに返るキー):
//!
//! ```text
//! id, name, username, host, avatarUrl, isLocked,
//! followersCount, followingCount, notesCount,
//! createdAt, description, bannerUrl, isBot, isCat
//! ```
//!
//! ## scope
//!
//! Misskey 仕様で `users/show` は **`read:account`** scope を要求する
//! (= `/api/i` と同じ最小権限)。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::json;

use crate::miauth::auth;
use crate::miauth::conv::from_actor_detailed;
use crate::miauth::error::{bad_request, internal_error};
use crate::state::AppState;

const SCOPE_READ_ACCOUNT: &str = "read:account";

#[derive(Debug, Deserialize, Default)]
pub struct UsersShowBody {
    #[serde(default)]
    pub i: Option<String>,
    /// 経路 1: `userId` 直接指定 (= 文字列化された Sakurasato 内部 `i64`)。
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    /// 経路 2: `username` + (option) `host`。host 省略 = 自インスタンス。
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

/// `POST /api/users/show` handler。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<UsersShowBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    // 1. userId 直接指定経路。
    let actor = if let Some(uid) = body.user_id.as_deref() {
        let Ok(id) = uid.parse::<i64>() else {
            return not_found("no such user");
        };
        match repo::actor::get_by_id(state.pool(), id).await {
            Ok(Some(a)) => a,
            Ok(None) => return not_found("no such user"),
            Err(err) => {
                tracing::error!(?err, user_id = uid, "miauth users/show: get_by_id failed");
                return internal_error("failed to look up user");
            }
        }
    } else if let Some(username) = body.username.as_deref() {
        // 2. username [+ host] 経路。host 省略 = local。
        let local_host = state.config().server.host.clone();
        // **PR #78 流儀** に揃えた lowercase lookup ── DB 列は case-sensitive。
        let user_lc = username.to_ascii_lowercase();
        let host_lc = body
            .host
            .as_deref()
            .filter(|h| !h.is_empty())
            .map_or(local_host, str::to_ascii_lowercase);
        match repo::actor::get_by_username_host(state.pool(), &user_lc, &host_lc).await {
            Ok(Some(a)) => a,
            Ok(None) => return not_found("no such user"),
            Err(err) => {
                tracing::error!(
                    ?err,
                    username,
                    "miauth users/show: get_by_username_host failed"
                );
                return internal_error("failed to look up user");
            }
        }
    } else {
        return bad_request("either userId or username is required");
    };

    let followers = repo::follow::count_followers(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    let following = repo::follow::count_following(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    // notes_count はお一人様 server なので local actor のみ集計可能。
    // remote actor の場合、Sakurasato 内に「相手が見せた note」しか持っていない
    // ので、その shard だけの count になる ── Misskey も同じ実装方針 (= cache
    // 経由なのでローカル view を返す)。
    let notes = if actor.is_local {
        repo::note::count_local(state.pool()).await.unwrap_or(0)
    } else {
        // 簡易: remote actor の note を count する関数は無いので 0 を返す。
        // wire 互換性的に「数 0」より「count を返さない」方が嬉しいケースもあるが、
        // wire shape に number が必要なので 0 を返して埋める。
        0
    };

    // `from_actor_detailed` 内の `from_actor_and_counts` が `actor.is_local` に
    // 応じて host を `None` (local) / `Some(actor.host)` (remote) に倒すため、
    // 本 handler は `ActorRow` をそのまま渡せば正しい host が得られる
    // (= `conv::timeline_entry_to_miss_note` のような上書きは不要)。
    let detailed = from_actor_detailed(&actor, followers, following, notes);
    Json(detailed).into_response()
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "NO_SUCH_USER",
                "message": message,
            },
        })),
    )
        .into_response()
}
