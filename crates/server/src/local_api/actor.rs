//! M13 PR1 (Issue #79): リモート/ローカル actor のプロフィール解決 + 関係取得。
//!
//! ## ルート
//!
//! - `GET /api/v1/actor?acct=user@host` | `?ap_id=https://...`
//!   ── actor を解決 (DB hit → `WebFinger` (acct のみ) → `fetch_and_upsert`) し、
//!   profile + ローカル actor との関係を返す。
//! - `GET /api/v1/actor/{id}` ── DB id で actor を取得 (= 既知 actor の再 fetch)。
//! - `GET /api/v1/actor/{id}/relationship` ── ローカル actor からの関係のみ。
//! - `GET /api/v1/actor/{id}/notes?limit=&before_id=` ── 当該 actor が author の
//!   Note を `note.id DESC` 順で列挙 (M13 PR3)。visibility filter:
//!   `public` / `unlisted` は常に見える、`followers` は viewer が accepted で
//!   follow しているとき、`direct` は viewer が `to_recipients` /
//!   `cc_recipients` に乗っているとき。author 自身を viewer にすると全件返る。
//!
//! ## 関係の意味
//!
//! - `following`: ローカル → target で `state = accepted` の follow 行が存在する。
//! - `follow_state`: ローカル → target の最新 follow 状態 (`pending`/`accepted`/`rejected`)。
//!   行が無ければ `null`。
//! - `followed_by`: target → ローカル で `state = accepted` の follow 行が存在する。
//!
//! 自分自身を引いたとき (= `actor.id == local.id`) は relationship をすべて
//! 中立 (`following=false`, `follow_state=null`, `followed_by=false`) で返す。
//!
//! ## セキュリティ
//!
//! `acct` 経由の解決は [`crate::webfinger_guard::ensure_webfinger_host_match`] で
//! host 一致を強制する (= cross-domain hijack 防御、PR #78 review F-1)。
//! `ap_id` 直接指定は `remote_actor::fetch_and_upsert` 内の `id == ap_id`
//! 自己整合性チェックと `net_guard` (SSRF 防御) に委ねる。

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::{ActorRow, FollowState};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::local_api::timeline::{self as timeline_api, ReactionSummaryDto, TimelineNote};
use crate::media_proxy_client::MediaProxyError;
use crate::remote_actor::{self, FetchError};
use crate::state::AppState;
use crate::webfinger_guard;

#[derive(Debug, Deserialize)]
pub struct LookupQuery {
    #[serde(default)]
    pub acct: Option<String>,
    #[serde(default)]
    pub ap_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActorWithRelationship {
    pub actor: ActorRow,
    pub relationship: Relationship,
}

#[derive(Debug, Serialize)]
pub struct ActorOnly {
    pub actor: ActorRow,
}

#[derive(Debug, Serialize, Clone)]
pub struct Relationship {
    pub following: bool,
    pub follow_state: Option<FollowState>,
    pub followed_by: bool,
    /// local → target の follow 行が `pending` / `accepted` のときの
    /// `follow.id`。`DELETE /api/v1/follow/{follow_id}` の引数に使う
    /// (= TUI Profile `f` キーが unfollow を撃つときの引数解決)。
    /// `rejected` / 行無しのときは `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_id: Option<i64>,
    /// ローカル actor が target をブロックしている (PR2、計画書 §5.7)。
    pub is_blocked: bool,
    /// ローカル actor → target の `block.id`。`is_blocked` のときのみ
    /// `Some` (`DELETE /api/v1/block/{block_id}` の引数に使う、PR6:
    /// TUI Profile 画面のブロックトグルが `follow_id` と同じ要領で使う)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<i64>,
    /// target がローカル actor をブロックしている (PR2、計画書 §5.7)。
    pub is_blocked_by: bool,
}

/// `GET /api/v1/actor?acct=...|ap_id=...`
///
/// 検索順:
/// 1. `ap_id` 指定 → `repo::actor::get_by_ap_id` で DB hit を見る。
///    miss なら `remote_actor::fetch_and_upsert(ap_id)` で取得 + DB 投入。
/// 2. `acct` 指定 → `WebFinger` (media-proxy 経由) → host 一致検証 →
///    `fetch_and_upsert(actor_uri)`。
/// 3. どちらも未指定 → 400。
pub async fn lookup(State(state): State<AppState>, Query(q): Query<LookupQuery>) -> Response {
    let actor = match (q.acct.as_deref(), q.ap_id.as_deref()) {
        (None, None) => {
            return bad_request("provide either `acct` or `ap_id` query parameter");
        }
        (_, Some(ap_id)) => match resolve_by_ap_id(&state, ap_id).await {
            Ok(actor) => actor,
            Err(err) => return err.into_response(),
        },
        (Some(acct), None) => match resolve_by_acct(&state, acct).await {
            Ok(actor) => actor,
            Err(err) => return err.into_response(),
        },
    };

    let relationship = match compute_relationship(&state, &actor).await {
        Ok(r) => r,
        Err(err) => {
            error!(
                ?err,
                target_id = actor.id,
                "relationship computation failed"
            );
            return internal_error();
        }
    };

    Json(ActorWithRelationship {
        actor,
        relationship,
    })
    .into_response()
}

/// `GET /api/v1/actor/{id}`
///
/// DB に既に取り込まれている actor を id で引く。remote fetch は行わない
/// (= lookup 経由で済ませる前提)。actor が存在しなければ 404。
pub async fn get_by_id(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let actor = match repo::actor::get_by_id(state.pool(), id).await {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(err) => {
            error!(?err, id, "get_by_id DB lookup failed");
            return internal_error();
        }
    };
    Json(ActorOnly { actor }).into_response()
}

/// `GET /api/v1/actor/{id}/relationship`
///
/// `actor.id` (= DB id) を指す actor とローカル actor の関係を返す。
/// actor が存在しなければ 404。ローカル actor が無い (init 未実行) は 503。
pub async fn relationship(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let actor = match repo::actor::get_by_id(state.pool(), id).await {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(err) => {
            error!(?err, id, "relationship target lookup failed");
            return internal_error();
        }
    };
    let rel = match compute_relationship(&state, &actor).await {
        Ok(r) => r,
        Err(err) => {
            error!(?err, id, "relationship computation failed");
            return err.into_response();
        }
    };
    Json(rel).into_response()
}

#[derive(Debug, Deserialize)]
pub struct NotesQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub before_id: Option<i64>,
}

/// プロフィール画面の note 一覧レスポンス。home timeline (`TimelineResponse`) と
/// 違い、特定 author の note を **id 降順** で並べるだけで renote は混ざらない
/// ため、カーソルは従来どおり `before_id` (note id) のまま据え置く。
#[derive(Debug, Serialize)]
pub struct AuthorNotesResponse {
    pub notes: Vec<TimelineNote>,
    pub next_before_id: Option<i64>,
}

/// `GET /api/v1/actor/{id}/notes?limit=&before_id=`
///
/// 当該 actor が author の Note を visibility filter 経由で列挙する。
/// - `id` が存在しなければ 404。
/// - ローカル actor が未 init なら 503。
/// - リアクション集計は **集計に失敗してもタイムライン本体は返す**
///   (= `home` と同じ動き、`warn!` だけ残す)。
pub async fn list_notes(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<NotesQuery>,
) -> Response {
    let target = match repo::actor::get_by_id(state.pool(), id).await {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(err) => {
            error!(?err, id, "list_notes: target actor lookup failed");
            return internal_error();
        }
    };
    let viewer = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(err) => return err.into_response(),
    };

    let limit = timeline_api::clamp_limit(q.limit);
    let entries = match repo::note::list_by_author(
        state.pool(),
        target.id,
        viewer.id,
        &viewer.ap_id,
        q.before_id,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, target_id = target.id, "list_notes: query failed");
            return internal_error();
        }
    };

    let host = &state.config().server.host;
    let next_before_id = entries.last().map(|e| e.id);

    // M8 PR3 と同じくリアクション集計を 1 クエリで取り、失敗時は warn だけ。
    // #151: announce 集計も同パターンで追加 (viewer = `viewer` = local actor)。
    let note_ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
    let mut by_note: HashMap<i64, Vec<ReactionSummaryDto>> = HashMap::new();
    match repo::reaction::counts_for_notes(state.pool(), &note_ids).await {
        Ok(rows) => {
            for row in rows {
                by_note
                    .entry(row.note_id)
                    .or_default()
                    .push(timeline_api::row_to_dto(host, row));
            }
        }
        Err(err) => {
            warn!(
                ?err,
                target_id = target.id,
                "list_notes: reaction counts_for_notes failed"
            );
        }
    }
    let mut announce_by_note: HashMap<i64, sakurasato_core::repo::announce::AnnounceSummaryRow> =
        HashMap::new();
    match repo::announce::counts_for_notes(state.pool(), &note_ids, viewer.id).await {
        Ok(rows) => {
            for row in rows {
                announce_by_note.insert(row.note_id, row);
            }
        }
        Err(err) => {
            warn!(
                ?err,
                target_id = target.id,
                "list_notes: announce counts_for_notes failed"
            );
        }
    }

    let notes: Vec<TimelineNote> = entries
        .into_iter()
        .map(|e| {
            let reactions = by_note.remove(&e.id).unwrap_or_default();
            let announce = announce_by_note.get(&e.id);
            // プロフィールは author の note のみ (renote 混在なし) なので None。
            TimelineNote::from_entry_with_aggregates(&e, reactions, announce, host, None)
        })
        .collect();

    Json(AuthorNotesResponse {
        notes,
        next_before_id,
    })
    .into_response()
}

async fn resolve_by_ap_id(state: &AppState, ap_id: &str) -> Result<ActorRow, ResolveError> {
    if let Some(existing) = repo::actor::get_by_ap_id(state.pool(), ap_id)
        .await
        .map_err(|err| ResolveError::Internal(format!("DB lookup: {err}")))?
    {
        return Ok(existing);
    }
    remote_actor::fetch_and_upsert(state, ap_id)
        .await
        .map_err(ResolveError::from_fetch)
}

async fn resolve_by_acct(state: &AppState, acct: &str) -> Result<ActorRow, ResolveError> {
    let expected_host = webfinger_guard::extract_acct_host(acct).ok_or_else(|| {
        ResolveError::BadRequest(format!("acct {acct:?} is not in `user@host` form"))
    })?;

    let resolved = state
        .media_proxy()
        .resolve_webfinger(acct)
        .await
        .map_err(|err| match err {
            MediaProxyError::Upstream {
                status,
                reason,
                message,
            } => ResolveError::BadGateway(format!(
                "media-proxy WebFinger resolve failed (HTTP {status}, reason={reason}): {message}",
            )),
            other => {
                ResolveError::BadGateway(format!("media-proxy WebFinger resolve failed: {other}"))
            }
        })?;

    webfinger_guard::ensure_webfinger_host_match(&expected_host, &resolved.actor_uri)
        .map_err(|err| ResolveError::BadRequest(format!("{err}")))?;

    if let Some(existing) = repo::actor::get_by_ap_id(state.pool(), &resolved.actor_uri)
        .await
        .map_err(|err| ResolveError::Internal(format!("DB lookup: {err}")))?
    {
        return Ok(existing);
    }
    remote_actor::fetch_and_upsert(state, &resolved.actor_uri)
        .await
        .map_err(ResolveError::from_fetch)
}

async fn compute_relationship(
    state: &AppState,
    target: &ActorRow,
) -> Result<Relationship, ResolveError> {
    let local = resolve_local_actor(state).await?;
    // follow ドメイン層の共通実装に委譲 ── 双方向の follow 行解釈 (viewer→target
    // 厳密 / target→viewer 緩い の非対称性含む) は [`crate::follow::compute_follow_relationship`]
    // に一元化し、MiAuth 経路 (`/api/users/show` の `isFollowed` 等) と共有する。
    let rel = crate::follow::compute_follow_relationship(state.pool(), local.id, target.id)
        .await
        .map_err(|err| ResolveError::Internal(format!("follow relationship: {err:#}")))?;
    // PR2 (計画書 §5.7): ブロック方向も follow と同じ (local, target) ペアで
    // 引く。自分自身が相手のときも `is_blocked`/`is_blocked_by` は false 相当
    // (block 行は自分自身を対象に作れない設計、create_block_core が拒否する)。
    // `block_id` (PR6 追加) は `get_by_pair` から直接取る ── `follow_id` と
    // 同じ要領で TUI のトグル操作の引数解決に使う。
    let block_id = repo::block::get_by_pair(state.pool(), local.id, target.id)
        .await
        .map_err(|err| ResolveError::Internal(format!("block lookup (out): {err}")))?
        .map(|b| b.id);
    let is_blocked_by = repo::block::is_blocked(state.pool(), target.id, local.id)
        .await
        .map_err(|err| ResolveError::Internal(format!("block lookup (in): {err}")))?;
    // 新規の pending 系 2 フィールドはローカル API wire shape には載せない
    // (= `Relationship` の JSON shape は従来どおり)。
    Ok(Relationship {
        following: rel.following,
        follow_state: rel.follow_state,
        followed_by: rel.followed_by,
        follow_id: rel.follow_id,
        is_blocked: block_id.is_some(),
        block_id,
        is_blocked_by,
    })
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, ResolveError> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(|err| ResolveError::Internal(format!("local actor lookup: {err}")))?
    {
        Some(a) if a.is_local => Ok(a),
        Some(_) => Err(ResolveError::Internal(format!(
            "actor for {user}@{host} exists but is not local",
        ))),
        None => Err(ResolveError::Unavailable(
            "local actor not initialized; run `sakurasato-server init` first".into(),
        )),
    }
}

#[derive(Debug)]
enum ResolveError {
    BadRequest(String),
    BadGateway(String),
    NotFound(String),
    Unavailable(String),
    Internal(String),
}

impl ResolveError {
    fn from_fetch(err: FetchError) -> Self {
        match err {
            FetchError::Blocked { host, reason } => {
                Self::BadRequest(format!("remote fetch blocked: host {host:?} → {reason}"))
            }
            FetchError::Malformed(msg) => {
                Self::BadGateway(format!("remote actor malformed: {msg}"))
            }
            FetchError::HttpStatus(s) if s.as_u16() == 404 => {
                Self::NotFound(format!("remote actor {s}"))
            }
            FetchError::HttpStatus(s) => Self::BadGateway(format!("remote actor HTTP {s}")),
            FetchError::Db(err) => Self::Internal(format!("upsert remote actor: {err}")),
            FetchError::RedirectRefused(loc) => {
                Self::BadGateway(format!("remote actor redirect refused: {loc}"))
            }
            FetchError::Timeout => Self::BadGateway("remote actor fetch timed out".into()),
            FetchError::TooLarge => Self::BadGateway("remote actor response too large".into()),
            other => Self::BadGateway(format!("remote actor fetch failed: {other}")),
        }
    }
}

impl IntoResponse for ResolveError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(serde_json::json!({ "error": body }))).into_response()
    }
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "actor not found" })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal server error" })),
    )
        .into_response()
}
