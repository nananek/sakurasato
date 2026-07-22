//! `POST /api/users/show` + `POST /api/users/search-by-username-and-host` +
//! `POST /api/users/search` (= M14 #159, 親 issue #150)。
//!
//! Misskey 互換クライアント (Milktea / `MissRirica` / Aria 等) が「ユーザを
//! 開く」「ユーザを検索する」経路。`show` は `userId` 直接指定と
//! `username` + `host` 指定の 2 経路をサポートする。
//!
//! ## clean-room
//!
//! - <https://api-doc.misskey.io/api/endpoints/users/show>
//! - <https://api-doc.misskey.io/api/endpoints/users/search-by-username-and-host>
//! - <https://api-doc.misskey.io/api/endpoints/users/search>
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
//! `search-by-username-and-host` / `search` はこのキー集合を持つオブジェクト
//! の配列を返す。前者はリスト機能の「メンバー追加」UI が `username` 前方
//! 一致で使う経路 (`misskey_dart` `MisskeyUsers.searchByUsernameAndHost`)、
//! 後者は一般ユーザー検索画面が `query` 部分一致 (username or display name)
//! で使う経路 (`misskey_dart` `MisskeyUsers.search`) ── どちらも未実装だと
//! Aria 側で API 呼び出しが失敗し例外を投げるため、リスト機能とセットで
//! 実装する。
//!
//! ## scope
//!
//! Misskey 仕様で `users/show` / `users/search-by-username-and-host` /
//! `users/search` はいずれも **`read:account`** scope を要求する
//! (= `/api/i` と同じ最小権限)。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::miauth::auth;
use crate::miauth::conv::from_actor_detailed;
use crate::miauth::error::{bad_request, error_resp, internal_error};
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
    /// 経路 3: `userIds` 一括指定 (= `MisskeyUsers.showByIds`、Aria の
    /// リストメンバー表示画面 `ListUsersNotifier` が使う)。指定時は他の
    /// フィールドより優先し、レスポンスは単一 object ではなく **配列** になる
    /// (Misskey 仕様、`userId`/`username` 経路と排他)。
    #[serde(rename = "userIds", default)]
    pub user_ids: Option<Vec<String>>,
}

/// `POST /api/users/show` handler。
///
/// `userIds` (配列) が指定された場合は一括取得経路 ── 見つからない id は
/// 404 にせず黙ってスキップする (Misskey 仕様。1 件消えているだけで
/// リスト全体の表示が壊れるのを避ける)。
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

    // 0. userIds 一括指定経路 (= 他経路より優先、レスポンス shape が異なる)。
    if let Some(ids) = body.user_ids.as_ref() {
        let ids: Vec<i64> = ids.iter().filter_map(|s| s.parse::<i64>().ok()).collect();
        let actors = match repo::actor::list_by_ids(state.pool(), &ids).await {
            Ok(v) => v,
            Err(err) => {
                tracing::error!(?err, "miauth users/show (batch): list_by_ids failed");
                return internal_error("failed to look up users");
            }
        };
        let mut out = Vec::with_capacity(actors.len());
        for actor in &actors {
            out.push(build_detailed_json(&state, actor).await);
        }
        return Json(out).into_response();
    }

    // 1. userId 直接指定経路。
    let actor = if let Some(uid) = body.user_id.as_deref() {
        let Ok(id) = uid.parse::<i64>() else {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
        };
        match repo::actor::get_by_id(state.pool(), id).await {
            Ok(Some(a)) => a,
            Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user"),
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
            Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user"),
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
        return bad_request("either userId, username, or userIds is required");
    };

    let detailed = build_detailed_json(&state, &actor).await;
    Json(detailed).into_response()
}

/// `ActorRow` → `UserDetailed` 相当の JSON。followers/following/notes count を
/// 都度引く (`users/show` の単発呼び出しでは無視できるコスト、`userIds` 一括
/// 経路でも Aria のリスト表示は数十件規模までなので N+1 の実害は薄い)。
///
/// `from_actor_detailed` 内の `from_actor_and_counts` が `actor.is_local` に
/// 応じて host を `None` (local) / `Some(actor.host)` (remote) に倒すため、
/// 本関数は `ActorRow` をそのまま渡せば正しい host が得られる
/// (= `conv::timeline_entry_to_miss_note` のような上書きは不要)。
async fn build_detailed_json(state: &AppState, actor: &ActorRow) -> JsonValue {
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
    from_actor_detailed(actor, followers, following, notes)
}

/// `limit` の既定値・上限。Misskey 公式仕様 (default 10, max 100) に揃える。
const SEARCH_LIMIT_DEFAULT: i64 = 10;
const SEARCH_LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct SearchByUsernameAndHostBody {
    #[serde(default)]
    pub i: Option<String>,
    /// 前方一致検索するローカル部分 (= `@` より前)。省略時は host のみで
    /// 絞る (= 空なら全 actor が対象)。
    #[serde(default)]
    pub username: Option<String>,
    /// 完全一致する host。省略時は local/remote 問わず全体から検索する。
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /api/users/search-by-username-and-host` handler。
///
/// リスト機能の「メンバー追加」で Aria 等が使うユーザー検索経路。
/// `username` の前方一致 (大小文字無視) + 任意 `host` 完全一致で
/// ローカル DB に存在する actor (= 過去に連合でやり取りした相手 + 自分自身)
/// を検索する。WebFinger 等の新規解決は行わない (= 既知 actor のみが対象)。
pub async fn search_by_username_and_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<SearchByUsernameAndHostBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    let username_pattern = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{}%", escape_ilike_pattern(s)));
    let host = body
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);
    let limit = body
        .limit
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .clamp(1, SEARCH_LIMIT_MAX);

    let actors = match repo::actor::search_by_username_host(
        state.pool(),
        username_pattern.as_deref(),
        host.as_deref(),
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                "miauth users/search-by-username-and-host: query failed"
            );
            return internal_error("user search failed");
        }
    };

    let mut out = Vec::with_capacity(actors.len());
    for actor in actors {
        let followers = repo::follow::count_followers(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let following = repo::follow::count_following(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let notes = if actor.is_local {
            repo::note::count_local(state.pool()).await.unwrap_or(0)
        } else {
            0
        };
        out.push(from_actor_detailed(&actor, followers, following, notes));
    }
    Json(out).into_response()
}

/// ILIKE パターンとして解釈される特殊文字 (`%` `_` `\`) をエスケープする。
/// 検索語に偶然これらの文字が含まれていても、意図せぬワイルドカード展開を
/// 起こさずリテラル一致として扱う (`PostgreSQL` の LIKE/ILIKE 既定エスケープ
/// 文字は `\`)。
fn escape_ilike_pattern(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

const SEARCH_QUERY_LIMIT_DEFAULT: i64 = 10;
const SEARCH_QUERY_LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct SearchBody {
    #[serde(default)]
    pub i: Option<String>,
    /// 検索語。`preferred_username` / `display_name` の部分一致に使う。
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// `"local"` / `"remote"` / `"combined"` (省略時 = combined)。
    #[serde(default)]
    pub origin: Option<String>,
}

/// `POST /api/users/search` handler。
///
/// Aria の一般ユーザー検索画面が使う経路 (`misskey_dart` `MisskeyUsers.search`)。
/// [`search_by_username_and_host`] と異なり、検索語は `preferred_username`
/// **または** `display_name` の部分一致で見る (= 検索ボックスへの自然な入力に
/// 合わせる)。`query` が空/欠落のときは 400 ではなく空配列を返す ── 検索
/// ボックスが空の瞬間にクライアントが呼んでも例外を投げさせないための
/// 安全側の挙動。
pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<SearchBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    let Some(query) = body
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Json(Vec::<serde_json::Value>::new()).into_response();
    };
    let pattern = format!("%{}%", escape_ilike_pattern(query));
    let origin_is_local = match body.origin.as_deref() {
        Some("local") => Some(true),
        Some("remote") => Some(false),
        _ => None,
    };
    let limit = body
        .limit
        .unwrap_or(SEARCH_QUERY_LIMIT_DEFAULT)
        .clamp(1, SEARCH_QUERY_LIMIT_MAX);
    let offset = body.offset.unwrap_or(0).max(0);

    let actors =
        match repo::actor::search(state.pool(), &pattern, origin_is_local, limit, offset).await {
            Ok(v) => v,
            Err(err) => {
                tracing::error!(?err, "miauth users/search: query failed");
                return internal_error("user search failed");
            }
        };

    let mut out = Vec::with_capacity(actors.len());
    for actor in actors {
        let followers = repo::follow::count_followers(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let following = repo::follow::count_following(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let notes = if actor.is_local {
            repo::note::count_local(state.pool()).await.unwrap_or(0)
        } else {
            0
        };
        out.push(from_actor_detailed(&actor, followers, following, notes));
    }
    Json(out).into_response()
}
