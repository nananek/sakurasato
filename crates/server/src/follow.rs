//! Follow / Unfollow の core ロジック (`sakurasato-server follow` CLI
//! と `POST|DELETE /api/v1/follow*` local API の共有実装)。
//!
//! # 経路
//!
//! - **CLI** (`Command::Follow` → [`run`]) ── `acct` or `--actor-uri` を受け、
//!   従来通り stdout に結果を出す。M10 で導入。
//! - **HTTP** (`POST /api/v1/follow`、`DELETE /api/v1/follow/{id}`) ──
//!   [`create_follow_core`] / [`delete_follow_core`] を直接呼ぶ。M13 PR2
//!   (Issue #79) で追加。TUI Profile 画面の `f` トグルがここを叩く。
//!
//! # `follow-cli-` prefix の意義
//!
//! 既存の `enqueue_auto_refollow` (M9 `dispatch/move_handler`) が
//! `follow-move-{old_follow_id}` を使っており、ここと衝突しない identifier に
//! することで「どこから生まれた Follow か」が `ap_id` だけで追える。CLI / API
//! 起源の Follow は (follower, followed) ペアで一意なので id に
//! `{follower}-{followed}` を含める ── 同じ相手に何度叩いても同じ activity id
//! になり (= 相手側で自然と冪等になる)、`(follower_actor_id,
//! followed_actor_id)` UNIQUE 制約とも整合する。
//!
//! # Undo Follow (M13 PR2)
//!
//! `DELETE /api/v1/follow/{id}` で送る outbound Undo Follow は決定論的
//! activity id `undo-follow-{follow_id}` を使う。CLI から同じ follow を二度
//! delete しても同じ id になり、相手側で重複排除される。inbound Undo Follow
//! は別途実装予定 ([[m12-66-complete]] 参照、本 PR では out のみ)。

use anyhow::Context;
use sakurasato_core::model::{ActorRow, FollowRow, FollowState};
use sakurasato_core::{Config, repo};
use serde_json::{Value as JsonValue, json};
use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

use crate::cli::FollowArgs;
use crate::delivery;
use crate::media_proxy_client::MediaProxyError;
use crate::remote_actor::{self, FetchError};
use crate::state::AppState;
use crate::webfinger_guard::{ensure_webfinger_host_match, extract_acct_host};

/// `create_follow_core` の入力。CLI / HTTP の双方が指定方式を選べる。
///
/// - [`FollowTarget::Acct`]: `WebFinger` (media-proxy 経由) で actor URI に解決し、
///   `remote_actor::fetch_and_upsert` で DB に取り込む。最も Mastodon-like。
/// - [`FollowTarget::ActorUri`]: `WebFinger` をスキップして直接 actor URI 指定。
///   `fetch_and_upsert` で DB に取り込む。
/// - [`FollowTarget::ActorId`]: 既に DB に居る actor を `id` で直接指定する。
///   remote fetch を一切行わないので、TUI が `GET /api/v1/actor` で取り込んだ
///   actor を follow するときや、テストで使う (= 実 network なしで核を検証)。
#[derive(Debug, Clone)]
pub enum FollowTarget {
    Acct(String),
    ActorUri(String),
    ActorId(i64),
}

/// `create_follow_core` / `delete_follow_core` の終端エラー。HTTP / CLI 双方で
/// 「クライアント由来 (= 400 / 404)」と「インフラ由来 (= 503)」を区別するため。
#[derive(Debug, Error)]
pub enum FollowError {
    /// 入力形式の問題 (acct パース失敗 / `actor_id` 不正 / `WebFinger` host 不一致 /
    /// remote が SSRF ガード違反 actor を返した、等)。HTTP は 400。
    #[error("{0}")]
    BadRequest(String),
    /// remote 側 (`WebFinger` / actor fetch) が失敗した。HTTP は 502。
    #[error("{0}")]
    BadGateway(String),
    /// 指定の `actor_id` / `follow_id` が DB に居ない、または `acct` を解決した
    /// が remote が 404 を返した。HTTP は 404。
    #[error("{0}")]
    NotFound(String),
    /// local actor 未 init / DB 不整合等の運用エラー。HTTP は 503。
    #[error("{0}")]
    Unavailable(String),
    /// 過去に Follow が reject されている、または自分自身を follow しようと
    /// したなど、状態的に拒否される操作。HTTP は 409。
    #[error("{0}")]
    Conflict(String),
    /// 認可エラー (= 削除しようとした follow の follower が local actor でない)。
    /// HTTP は 403。
    #[error("{0}")]
    Forbidden(String),
    /// DB / シリアライズ等の内部エラー。HTTP は 503 + 詳細はログのみ。
    #[error(transparent)]
    Internal(anyhow::Error),
}

impl FollowError {
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
            FetchError::Db(e) => {
                Self::Internal(anyhow::Error::new(e).context("upsert remote actor"))
            }
            FetchError::RedirectRefused(loc) => {
                Self::BadGateway(format!("remote actor redirect refused: {loc}"))
            }
            FetchError::Timeout => Self::BadGateway("remote actor fetch timed out".into()),
            FetchError::TooLarge => Self::BadGateway("remote actor response too large".into()),
            other => Self::BadGateway(format!("remote actor fetch failed: {other}")),
        }
    }

    fn from_media_proxy(err: MediaProxyError) -> Self {
        match err {
            MediaProxyError::Upstream {
                status,
                reason,
                message,
            } => Self::BadGateway(format!(
                "media-proxy WebFinger resolve failed (HTTP {status}, reason={reason}): {message}",
            )),
            other => Self::BadGateway(format!("media-proxy WebFinger resolve failed: {other}")),
        }
    }
}

impl From<sqlx::Error> for FollowError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

/// `create_follow_core` の結果。`queue_id` / `inbox_url` が `None` になる
/// ケースは 3 つあり、3 つの bool フラグで意味を明確に区別する:
///
/// 1. `already_accepted = true` ── 既存 `accepted` 行を再叩き。Follow を再送
///    しない。
/// 2. `already_pending = true` (Issue #113) ── 既存 `pending` 行を再叩き。
///    Follow を再送せず、配送 worker の retry に任せる。明示 retry したい
///    ときは `sakurasato-server deliver --queue-id N`。
/// 3. 上 2 つとも `false` で `queue_id = Some` ── 新規 INSERT または
///    `rejected → pending` 復活で Follow を `delivery_queue` に積んだ正常系。
#[derive(Debug, Clone)]
pub struct FollowOutcome {
    pub follow: FollowRow,
    pub target: ActorRow,
    pub queue_id: Option<i64>,
    pub inbox_url: Option<String>,
    /// 既存 `accepted` 行を再叩き ── 何もせず idempotent 成功で返す印。
    pub already_accepted: bool,
    /// **Issue #113**: 既存 `pending` 行を再叩き ── enqueue 抑止の印。
    pub already_pending: bool,
}

/// `delete_follow_core` の結果。Undo Follow activity を必ず 1 行 enqueue する。
#[derive(Debug, Clone)]
pub struct UnfollowOutcome {
    pub follow_id: i64,
    pub target_ap_id: String,
    pub queue_id: i64,
    pub inbox_url: String,
}

/// viewer (= ローカル actor) から見た target との双方向 follow 関係。
///
/// `local_api::actor::Relationship` (TUI 向け `GET /api/v1/actor/{id}/relationship`)
/// と同じ意味論を follow ドメイン層に切り出し、MiAuth 経路 (`/api/users/show`
/// 等の Misskey `UserDetailedNotMe` 必須フィールド `isFollowing` /
/// `isFollowed` / `hasPendingFollowRequestFromYou` / `hasPendingFollowRequestToYou`)
/// からも共有できるようにしたもの (PR #348 系、`#150` 系 follow-relationship)。
///
/// 意味:
///
/// - `following`: viewer → target が `accepted`
/// - `follow_state`: viewer → target の最新 state (`pending`/`accepted`/`rejected`)、
///   行が無ければ `None`
/// - `followed_by`: target → viewer が `accepted`
/// - `follow_id`: viewer → target の follow 行が `pending` / `accepted` のとき
///   の `follow.id` (`rejected` / 行無しは `None`)
/// - `has_pending_follow_request_from_you`: viewer → target が `pending`
/// - `has_pending_follow_request_to_you`: target → viewer が `pending`
///   (= M12 鍵アカ運用で相手の承認待ち)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Misskey UserDetailedNotMe の required bool 4 件 + 既存 Relationship の follow_id を 1 構造体で運ぶ設計"
)]
pub struct FollowRelationship {
    pub following: bool,
    pub follow_state: Option<FollowState>,
    pub followed_by: bool,
    pub follow_id: Option<i64>,
    /// viewer→target が `pending`。
    pub has_pending_follow_request_from_you: bool,
    /// target→viewer が `pending`。
    pub has_pending_follow_request_to_you: bool,
}

impl FollowRelationship {
    /// 全フィールド中立 (= follow 行が無い / 自分自身)。
    pub fn neutral() -> Self {
        Self {
            following: false,
            follow_state: None,
            followed_by: false,
            follow_id: None,
            has_pending_follow_request_from_you: false,
            has_pending_follow_request_to_you: false,
        }
    }
}

/// `(viewer, target)` の双方向 follow 行を引いて [`FollowRelationship`] を計算する。
///
/// `viewer_actor_id == target_actor_id` (自分自身) は DB を引かず [`FollowRelationship::neutral`]
/// を返す (= `local_api::actor::compute_relationship` と同じ方針)。
///
/// 解釈の非対称性は `local_api::actor.rs` の既存実装をそのまま引き継ぐ:
///
/// - viewer→target 方向は厳密 parse (`pending` / `accepted` / `rejected` 以外は
///   Err ── 我々の書いた行なので unknown state は即座に気付きたい)。
/// - target→viewer 方向は `matches!` の緩い文字列判定 (remote 由来の行を弾くと
///   `followed_by` が偽陰性になり得るため、既存の意図的挙動を変えない)。
pub async fn compute_follow_relationship(
    pool: &PgPool,
    viewer_actor_id: i64,
    target_actor_id: i64,
) -> anyhow::Result<FollowRelationship> {
    if viewer_actor_id == target_actor_id {
        return Ok(FollowRelationship::neutral());
    }

    let local_to_target = repo::follow::get_by_pair(pool, viewer_actor_id, target_actor_id)
        .await
        .context("follow lookup (out)")?;
    let target_to_local = repo::follow::get_by_pair(pool, target_actor_id, viewer_actor_id)
        .await
        .context("follow lookup (in)")?;

    let follow_state = match local_to_target.as_ref() {
        Some(row) => Some(parse_state_str(&row.state)?),
        None => None,
    };
    let following = follow_state == Some(FollowState::Accepted);
    // `pending` / `accepted` のときだけ follow_id を露出する。`rejected` は
    // unfollow 不要 (= フォロー関係としては成立していない) かつ、`DELETE
    // /follow/{id}` で削除すると次に follow した時に `upsert_pending` 経由で
    // `pending` に復活する設計と整合しなくなるため。
    let follow_id = match local_to_target.as_ref() {
        Some(row) if matches!(row.state.as_str(), "pending" | "accepted") => Some(row.id),
        _ => None,
    };
    let followed_by = matches!(
        target_to_local.as_ref().map(|r| r.state.as_str()),
        Some("accepted")
    );

    Ok(FollowRelationship {
        following,
        follow_state,
        followed_by,
        follow_id,
        has_pending_follow_request_from_you: follow_state == Some(FollowState::Pending),
        has_pending_follow_request_to_you: matches!(
            target_to_local.as_ref().map(|r| r.state.as_str()),
            Some("pending")
        ),
    })
}

/// `follow.state` の文字列を [`FollowState`] に厳密 parse する。
/// private の [`parse_follow_state`] (row 単位・`FollowError` 返却) と名前衝突
/// しないための別名。unknown state は DB 不整合なので Err にする。
fn parse_state_str(raw: &str) -> anyhow::Result<FollowState> {
    match raw {
        "pending" => Ok(FollowState::Pending),
        "accepted" => Ok(FollowState::Accepted),
        "rejected" => Ok(FollowState::Rejected),
        other => anyhow::bail!("follow row has unknown state {other:?}"),
    }
}

/// **M13 PR2 core**: 指定 target を follow する。
///
/// 流れ ([`run`] の本体と同じだが state-driven にして HTTP からも呼べる形)。
/// 1. local actor 解決。
/// 2. target を `FollowTarget` 別に解決 (DB hit / `WebFinger` / `fetch_and_upsert`)。
///    `Acct` 経路は [`ensure_webfinger_host_match`] で cross-domain hijack を弾く。
/// 3. self-follow / `rejected` 復活ガード / 既存 `accepted` short-circuit。
/// 4. 決定論的 `follow-cli-{follower}-{followed}` で `upsert_pending` + enqueue。
#[allow(
    clippy::too_many_lines,
    reason = "follow フロー全体 (validate / pre-check / upsert / enqueue) を 1 関数で抱える"
)]
pub async fn create_follow_core(
    state: &AppState,
    target: FollowTarget,
) -> Result<FollowOutcome, FollowError> {
    let local = resolve_local_actor(state).await?;
    let target_actor = resolve_target_actor(state, target).await?;

    if target_actor.id == local.id {
        return Err(FollowError::Conflict(format!(
            "refusing to follow our own local actor {:?}",
            local.ap_id,
        )));
    }
    if let Some(moved) = target_actor.moved_to_ap_id.as_deref() {
        // moved_to が立っている actor を follow するのは設計上問題ないが、
        // 明示的に follow する意図は薄いので警告だけ出す (= ブロックはしない)。
        warn!(
            target = %target_actor.ap_id,
            moved_to = moved,
            "target actor has movedTo set; consider following the new actor instead",
        );
    }

    let follow_ap_id = build_follow_ap_id(state, &local, target_actor.id);

    // **Issue #113**: pending 行が既に存在するとき、`upsert_pending` は ON
    // CONFLICT で同行を返すだけだが、後続の `enqueue_activity` を毎回叩いて
    // しまい delivery_queue に同 activity が累積、配送 worker が成功するまで
    // 相手側 inbox に Follow を投げ続けて重複 follow request が残る。事前に
    // `get_by_pair` で既存行を確認し、pending なら early return で
    // **enqueue を抑止** する (= worker の retry に任せる、明示 retry したい
    // ときは `sakurasato-server deliver --queue-id N` で個別 flush)。
    if let Some(existing) = repo::follow::get_by_pair(state.pool(), local.id, target_actor.id)
        .await
        .map_err(|e| {
            FollowError::Internal(anyhow::Error::new(e).context("get_by_pair before follow upsert"))
        })?
    {
        match parse_follow_state(&existing)? {
            FollowState::Accepted => {
                return Ok(FollowOutcome {
                    follow: existing,
                    target: target_actor,
                    queue_id: None,
                    inbox_url: None,
                    already_accepted: true,
                    already_pending: false,
                });
            }
            FollowState::Pending => {
                info!(
                    follow_id = existing.id,
                    target = %target_actor.ap_id,
                    "existing pending follow row; not enqueueing duplicate (worker will retry)",
                );
                return Ok(FollowOutcome {
                    follow: existing,
                    target: target_actor,
                    queue_id: None,
                    inbox_url: None,
                    already_accepted: false,
                    already_pending: true,
                });
            }
            FollowState::Rejected => {
                // 復活経路は `upsert_pending` の ON CONFLICT で rejected →
                // pending に倒す。fall through で下の `upsert_pending` に進む。
            }
        }
    }

    let row = repo::follow::upsert_pending(state.pool(), &follow_ap_id, local.id, target_actor.id)
        .await
        .map_err(|e| {
            FollowError::Internal(anyhow::Error::new(e).context("upsert pending follow row"))
        })?;

    match parse_follow_state(&row)? {
        FollowState::Accepted => {
            // get_by_pair → upsert_pending の間に並行 Accept が来た稀なレース。
            // 安全側で accepted 扱いで返す (= idempotent)。
            return Ok(FollowOutcome {
                follow: row,
                target: target_actor,
                queue_id: None,
                inbox_url: None,
                already_accepted: true,
                already_pending: false,
            });
        }
        FollowState::Rejected => {
            // upsert_pending で rejected → pending 復活を期待したが、その復活
            // ロジックがコールド側で動かなかった場合の保険。
            return Err(FollowError::Conflict(format!(
                "previous Follow to {target} was rejected and not revived; \
                 inspect follow row id={id}",
                target = target_actor.ap_id,
                id = row.id,
            )));
        }
        FollowState::Pending => {
            // 期待ケース: 新規 INSERT または rejected → pending 復活。
        }
    }

    let activity = build_follow_activity(&follow_ap_id, &local.ap_id, &target_actor.ap_id);
    let inbox = target_actor
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target_actor.inbox_url)
        .to_string();
    let queued = delivery::enqueue_activity(state.pool(), local.id, &inbox, &activity)
        .await
        .map_err(|e| FollowError::Internal(e.context(format!("enqueue Follow to {inbox}"))))?;
    state.wake_delivery();

    Ok(FollowOutcome {
        follow: row,
        target: target_actor,
        queue_id: Some(queued.id),
        inbox_url: Some(inbox),
        already_accepted: false,
        already_pending: false,
    })
}

/// **M13 PR2 core**: 既存 follow 行を取り消す (outbound Undo Follow + 行削除)。
///
/// 認可: `follower_actor_id` が local actor の id と一致する行のみ削除可能。
/// 一致しなければ [`FollowError::Forbidden`]。state を問わず削除する
/// (`pending` / `accepted` / `rejected` のいずれであっても、ユーザが「もう
/// 関係を絶ちたい」と判断したら Undo を送って消す)。
///
/// 配送と DB 削除は単一トランザクションで囲む ── enqueue が成功しても
/// commit 前に panic / DB エラーで rollback されれば、Undo 送出も follow
/// 行削除も両方とも巻き戻り、運用上の一貫性が保たれる。
pub async fn delete_follow_core(
    state: &AppState,
    follow_id: i64,
) -> Result<UnfollowOutcome, FollowError> {
    let local = resolve_local_actor(state).await?;
    let row = repo::follow::get_by_id(state.pool(), follow_id)
        .await?
        .ok_or_else(|| FollowError::NotFound(format!("no follow row with id={follow_id}")))?;
    if row.follower_actor_id != local.id {
        return Err(FollowError::Forbidden(format!(
            "follow id={} is not owned by the local actor (follower_actor_id={})",
            row.id, row.follower_actor_id,
        )));
    }
    let target = repo::actor::get_by_id(state.pool(), row.followed_actor_id)
        .await?
        .ok_or_else(|| {
            FollowError::Internal(anyhow::anyhow!(
                "follow row id={} references missing target actor",
                row.id,
            ))
        })?;

    let undo_ap_id = format!(
        "https://{host}/users/{user}/activities/undo-follow-{follow_id}",
        host = state.config().server.host,
        user = local.preferred_username,
    );
    let inbox = target
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target.inbox_url)
        .to_string();
    let activity = build_undo_follow_activity(&undo_ap_id, &local.ap_id, &row, &target.ap_id);

    let mut tx =
        state.pool().begin().await.map_err(|e| {
            FollowError::Internal(anyhow::Error::new(e).context("begin transaction"))
        })?;
    let queued = delivery::enqueue_activity(&mut *tx, local.id, &inbox, &activity)
        .await
        .map_err(|e| FollowError::Internal(e.context(format!("enqueue Undo Follow to {inbox}"))))?;
    sqlx::query!("DELETE FROM follow WHERE id = $1", row.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            FollowError::Internal(anyhow::Error::new(e).context("delete follow row in tx"))
        })?;
    tx.commit().await.map_err(|e| {
        FollowError::Internal(anyhow::Error::new(e).context("commit unfollow transaction"))
    })?;
    // commit 後に wake する ── tx 内 enqueue した行は commit 前は他コネクション
    // から見えないので、ワーカを早く起こしても pick_due が拾えず空振りになる。
    state.wake_delivery();

    info!(
        follow_id = row.id,
        target = %target.ap_id,
        queue_id = queued.id,
        "Undo Follow queued; follow row deleted",
    );
    Ok(UnfollowOutcome {
        follow_id: row.id,
        target_ap_id: target.ap_id,
        queue_id: queued.id,
        inbox_url: inbox,
    })
}

/// CLI `sakurasato-server follow <acct>` の入口。`run_with_state` パターンで
/// state 生成と core ロジックを分け、core ([`create_follow_core`]) を HTTP
/// 経路と共有する。
pub async fn run(config: Config, args: FollowArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    run_with_state(&state, args).await
}

/// CLI 本体 (テスト / HTTP 共有経路から呼べる形)。
pub async fn run_with_state(state: &AppState, args: FollowArgs) -> anyhow::Result<()> {
    let target = if let Some(uri) = args.actor_uri.as_deref() {
        info!(
            actor_uri = uri,
            "follow: using --actor-uri (skipping WebFinger)"
        );
        FollowTarget::ActorUri(uri.to_string())
    } else {
        // `--actor-uri` 無しのときは clap 側で `acct` が `required_unless_present`
        // により必須。空文字で fall through した場合は core 側 `parse_acct` で
        // 弾く設計。
        FollowTarget::Acct(args.acct.unwrap_or_default())
    };
    let outcome = create_follow_core(state, target)
        .await
        .map_err(|e| match e {
            FollowError::Internal(err) => err,
            other => anyhow::anyhow!("{other}"),
        })?;

    if outcome.already_accepted {
        println!(
            "already following {target}: follow_id={id} state=accepted (no Follow sent)",
            target = outcome.target.ap_id,
            id = outcome.follow.id,
        );
        return Ok(());
    }
    if outcome.already_pending {
        // **Issue #113**: 既存 pending 行への再叩き ── 重複は enqueue しない。
        // ただし CLI は daemon とは別プロセスで daemon ワーカを起床できないので、
        // 据え置きの pending 行を自プロセスで即 flush する (#211 cross-process
        // 回帰対応)。
        delivery::flush_due_now(state).await;
        println!(
            "follow already pending for {target}: follow_id={id} state=pending \
             (flushed inline; worker will retry if delivery failed)",
            target = outcome.target.ap_id,
            id = outcome.follow.id,
        );
        return Ok(());
    }
    let queue_id = outcome
        .queue_id
        .ok_or_else(|| anyhow::anyhow!("internal: pending Follow returned without queue_id"))?;
    let inbox = outcome.inbox_url.unwrap_or_default();
    // CLI は別プロセスなので daemon ワーカは `wake_delivery` で起床しない。
    // 積んだ Follow を自プロセスで即配送する (#211 cross-process 回帰対応)。
    delivery::flush_due_now(state).await;
    println!(
        "queued Follow to {target}: follow_id={follow_id} delivery_queue_id={queue_id} inbox={inbox}",
        target = outcome.target.ap_id,
        follow_id = outcome.follow.id,
    );
    Ok(())
}

/// `FollowTarget` を `ActorRow` に解決する。`ActorId` 経路は remote fetch を
/// 行わず DB lookup のみ。
async fn resolve_target_actor(
    state: &AppState,
    target: FollowTarget,
) -> Result<ActorRow, FollowError> {
    match target {
        FollowTarget::ActorId(id) => {
            let row = repo::actor::get_by_id(state.pool(), id)
                .await?
                .ok_or_else(|| {
                    FollowError::NotFound(format!("no actor with id={id} in local DB"))
                })?;
            Ok(row)
        }
        FollowTarget::ActorUri(uri) => remote_actor::fetch_and_upsert(state, &uri)
            .await
            .map_err(FollowError::from_fetch),
        FollowTarget::Acct(raw) => {
            let acct = raw.trim();
            if acct.is_empty() {
                return Err(FollowError::BadRequest(
                    "acct is empty; supply `user@host`".into(),
                ));
            }
            let expected_host = extract_acct_host(acct).ok_or_else(|| {
                FollowError::BadRequest(format!("acct {acct:?} is not in `user@host` form"))
            })?;
            let resolved = state
                .media_proxy()
                .resolve_webfinger(acct)
                .await
                .map_err(FollowError::from_media_proxy)?;
            info!(
                acct = %acct,
                subject = %resolved.subject,
                actor_uri = %resolved.actor_uri,
                "follow: WebFinger resolved",
            );
            ensure_webfinger_host_match(&expected_host, &resolved.actor_uri)
                .map_err(|e| FollowError::BadRequest(format!("{e}")))?;
            remote_actor::fetch_and_upsert(state, &resolved.actor_uri)
                .await
                .map_err(FollowError::from_fetch)
        }
    }
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, FollowError> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await? {
        Some(a) if a.is_local => Ok(a),
        Some(_) => Err(FollowError::Unavailable(format!(
            "actor for {user}@{host} exists but is not local; DB inconsistent",
        ))),
        None => Err(FollowError::Unavailable(format!(
            "no local actor for {user}@{host}; run `sakurasato-server init` first",
        ))),
    }
}

fn parse_follow_state(row: &FollowRow) -> Result<FollowState, FollowError> {
    match row.state.as_str() {
        "pending" => Ok(FollowState::Pending),
        "accepted" => Ok(FollowState::Accepted),
        "rejected" => Ok(FollowState::Rejected),
        other => Err(FollowError::Internal(anyhow::anyhow!(
            "follow row {} has unknown state {other:?}",
            row.id,
        ))),
    }
}

fn build_follow_ap_id(state: &AppState, local: &ActorRow, target_actor_id: i64) -> String {
    format!(
        "https://{host}/users/{user}/activities/follow-cli-{follower}-{followed}",
        host = state.config().server.host,
        user = local.preferred_username,
        follower = local.id,
        followed = target_actor_id,
    )
}

fn build_follow_activity(ap_id: &str, actor: &str, object: &str) -> JsonValue {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": ap_id,
        "type": "Follow",
        "actor": actor,
        "object": object,
    })
}

/// Undo Follow activity を組み立てる。`object` には元 Follow の inline 形式
/// (= `id` / `type` / `actor` / `object`) を埋め込む ── Mastodon / Misskey
/// 双方ともこの inline 形式を受理し、`id` で元 Follow 行を検索する。
fn build_undo_follow_activity(
    undo_ap_id: &str,
    local_ap_id: &str,
    follow_row: &FollowRow,
    target_ap_id: &str,
) -> JsonValue {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_ap_id,
        "type": "Undo",
        "actor": local_ap_id,
        "object": {
            "id": follow_row.ap_id,
            "type": "Follow",
            "actor": local_ap_id,
            "object": target_ap_id,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fake_row(id: i64, state: &str) -> FollowRow {
        FollowRow {
            id,
            ap_id: format!("https://x/users/me/activities/follow-cli-1-{id}"),
            follower_actor_id: 1,
            followed_actor_id: id,
            state: state.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn parse_follow_state_handles_known_values() {
        assert_eq!(
            parse_follow_state(&fake_row(2, "pending")).unwrap(),
            FollowState::Pending,
        );
        assert_eq!(
            parse_follow_state(&fake_row(2, "accepted")).unwrap(),
            FollowState::Accepted,
        );
        assert_eq!(
            parse_follow_state(&fake_row(2, "rejected")).unwrap(),
            FollowState::Rejected,
        );
    }

    #[test]
    fn parse_follow_state_errors_on_unknown() {
        let err = parse_follow_state(&fake_row(7, "weird")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown state"), "msg={msg}");
        assert!(msg.contains("\"weird\""), "msg={msg}");
    }

    #[test]
    fn build_follow_activity_has_required_fields() {
        let a = build_follow_activity(
            "https://x/users/me/activities/follow-cli-1-2",
            "https://x/users/me",
            "https://y/users/bob",
        );
        assert_eq!(a["type"], "Follow");
        assert_eq!(a["id"], "https://x/users/me/activities/follow-cli-1-2");
        assert_eq!(a["actor"], "https://x/users/me");
        assert_eq!(a["object"], "https://y/users/bob");
        assert_eq!(a["@context"], "https://www.w3.org/ns/activitystreams");
    }

    #[test]
    fn build_undo_follow_activity_wraps_inline_follow() {
        let row = fake_row(42, "accepted");
        let undo = build_undo_follow_activity(
            "https://x/users/me/activities/undo-follow-42",
            "https://x/users/me",
            &row,
            "https://y/users/bob",
        );
        assert_eq!(undo["type"], "Undo");
        assert_eq!(undo["id"], "https://x/users/me/activities/undo-follow-42");
        assert_eq!(undo["actor"], "https://x/users/me");
        assert_eq!(undo["object"]["type"], "Follow");
        assert_eq!(undo["object"]["id"], row.ap_id);
        assert_eq!(undo["object"]["actor"], "https://x/users/me");
        assert_eq!(undo["object"]["object"], "https://y/users/bob");
        assert_eq!(undo["@context"], "https://www.w3.org/ns/activitystreams");
    }

    // ── compute_follow_relationship (DB テスト) ─────────────────────────────

    fn fake_new_actor(id_suffix: &str, host: &str, is_local: bool) -> repo::actor::NewActor {
        repo::actor::NewActor {
            ap_id: format!("https://{host}/users/user{id_suffix}"),
            preferred_username: format!("user{id_suffix}"),
            host: host.into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: format!("https://{host}/inbox"),
            shared_inbox_url: Some(format!("https://{host}/shared-inbox")),
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: format!("https://{host}/users/user{id_suffix}#main-key"),
            public_key_pem: "fake-public-key".into(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: vec![],
            moved_to_ap_id: None,
            is_local,
            actor_type: "Person".into(),
            manually_approves_followers: false,
        }
    }

    /// follow 行を `(follower, followed, state)` で直接 INSERT する。
    #[allow(
        clippy::similar_names,
        reason = "AP 用語の follower/followed は命名衝突に非ず"
    )]
    async fn seed_follow(pool: &PgPool, follower: i64, followed: i64, state: FollowState) {
        repo::follow::insert_pending(
            pool,
            &format!("https://x.test/activities/follow-{follower}-{followed}"),
            follower,
            followed,
        )
        .await
        .unwrap();
        // `insert_pending` は ap_id で冪等なので、同じペアで複数回呼んでも
        // `get_by_ap_id` が同じ行を返す ── state を上書きしてから進める。
        let row = repo::follow::get_by_pair(pool, follower, followed)
            .await
            .unwrap()
            .unwrap();
        repo::follow::set_state(pool, row.id, state).await.unwrap();
    }

    async fn seed_two_actors(pool: &PgPool) -> (i64, i64) {
        let viewer = repo::actor::insert(pool, fake_new_actor("1", "sakurasato.test", true))
            .await
            .unwrap();
        let target = repo::actor::insert(pool, fake_new_actor("2", "remote.test", false))
            .await
            .unwrap();
        (viewer.id, target.id)
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_is_neutral_for_self(pool: PgPool) {
        let (viewer, _) = seed_two_actors(&pool).await;
        let rel = compute_follow_relationship(&pool, viewer, viewer)
            .await
            .unwrap();
        assert_eq!(rel, FollowRelationship::neutral());
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_is_neutral_without_rows(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert_eq!(rel, FollowRelationship::neutral());
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_viewer_to_target_pending(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, viewer, target, FollowState::Pending).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(!rel.following);
        assert_eq!(rel.follow_state, Some(FollowState::Pending));
        assert!(!rel.followed_by);
        assert!(rel.follow_id.is_some());
        assert!(rel.has_pending_follow_request_from_you);
        assert!(!rel.has_pending_follow_request_to_you);
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_viewer_to_target_accepted(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, viewer, target, FollowState::Accepted).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(rel.following);
        assert_eq!(rel.follow_state, Some(FollowState::Accepted));
        assert!(!rel.followed_by);
        assert!(rel.follow_id.is_some());
        assert!(!rel.has_pending_follow_request_from_you);
        assert!(!rel.has_pending_follow_request_to_you);
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_viewer_to_target_rejected(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, viewer, target, FollowState::Rejected).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(!rel.following);
        assert_eq!(rel.follow_state, Some(FollowState::Rejected));
        assert!(!rel.followed_by);
        assert_eq!(
            rel.follow_id, None,
            "rejected row must not expose follow_id"
        );
        assert!(!rel.has_pending_follow_request_from_you);
        assert!(!rel.has_pending_follow_request_to_you);
    }

    /// **鍵アカ (M12) 承認待ち**: target → viewer が pending (= 相手からの
    /// Follow が我々側で承認待ち)。今回のユーザー報告シナリオの核心。
    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_target_to_viewer_pending(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, target, viewer, FollowState::Pending).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(!rel.following);
        assert_eq!(rel.follow_state, None);
        assert!(!rel.followed_by);
        assert_eq!(rel.follow_id, None);
        assert!(!rel.has_pending_follow_request_from_you);
        assert!(rel.has_pending_follow_request_to_you);
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_target_to_viewer_accepted(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, target, viewer, FollowState::Accepted).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(!rel.following);
        assert_eq!(rel.follow_state, None);
        assert!(rel.followed_by);
        assert!(!rel.has_pending_follow_request_from_you);
        assert!(!rel.has_pending_follow_request_to_you);
    }

    #[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
    async fn compute_follow_relationship_mutual_accepted(pool: PgPool) {
        let (viewer, target) = seed_two_actors(&pool).await;
        seed_follow(&pool, viewer, target, FollowState::Accepted).await;
        seed_follow(&pool, target, viewer, FollowState::Accepted).await;
        let rel = compute_follow_relationship(&pool, viewer, target)
            .await
            .unwrap();
        assert!(rel.following);
        assert_eq!(rel.follow_state, Some(FollowState::Accepted));
        assert!(rel.followed_by);
        assert!(rel.follow_id.is_some());
        assert!(!rel.has_pending_follow_request_from_you);
        assert!(!rel.has_pending_follow_request_to_you);
    }
}
