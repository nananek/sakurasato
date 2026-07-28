//! `sakurasato follow-request list/approve/reject` CLI (Issue #66 / M12)。
//!
//! 鍵アカ運用 (`actor.manually_approves_followers = TRUE`) で `pending` のまま
//! 据え置かれた inbound Follow を、管理者が明示的に承認 / 拒否するための
//! 管理操作。
//!
//! # 流れ
//!
//! - `list` ── `repo::follow::list_pending_for_local` で `follow.state =
//!   'pending'` かつ followed が local actor の行を列挙。
//! - `approve --id N` ── 行を fetch し、followed が local actor であることを
//!   確認 → Accept activity を `delivery_queue` に積み → `set_state(accepted)`。
//! - `reject --id N` ── 同上で Reject activity 配送 + `set_state(rejected)`。
//!
//! # Accept / Reject の `object`
//!
//! 仕様上 `object` は元 Follow URI (文字列) でも inline Follow object でも
//! 可。互換性のため Mastodon / Misskey どちらも inline を好む傾向にあり、
//! 通常経路の inbound Follow ハンドラ ([`crate::dispatch::handler`]) も
//! inline で組んでいる。本 CLI は元 activity 本文を保管していないので、
//! `(follow.ap_id, follower.ap_id, followed.ap_id)` から最小の Follow JSON
//! を再構成して `object` に埋める。Mastodon / Misskey とも `id` 一致で
//! follow を引くため、この最小形でも問題なく処理される。

// follow.{follower,followed}_actor_id / 同名の actor 変数は AP 用語で
// 自然な命名。alias でリネームすると逆に読みづらいので、`follow` 系
// モジュール (`crates/core/src/repo/follow.rs` と同じ理由) で許可する。
#![allow(clippy::similar_names)]

use anyhow::Context;
use sakurasato_core::model::{ActorRow, FollowRow, FollowState};
use sakurasato_core::{Config, repo};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tracing::info;

use crate::cli::{FollowRequestArgs, FollowRequestCommand};
use crate::delivery;
use crate::state::AppState;

/// Accept / Reject の 2 値で十分な内部表現。`&'static str` だと将来の追加で
/// `panic!` 経路が増えがちなため型安全に絞る ([round-1 review #5] 対応)。
#[derive(Debug, Clone, Copy)]
enum ResponseType {
    Accept,
    Reject,
}

impl ResponseType {
    fn as_type(self) -> &'static str {
        match self {
            Self::Accept => "Accept",
            Self::Reject => "Reject",
        }
    }
    fn id_suffix(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Accept => "approved",
            Self::Reject => "rejected",
        }
    }
}

/// `approve_or_reject` のエラー区分 ([round-1 review #3] 対応)。
///
/// CLI / local API の双方で「クライアント由来 (= 400)」と「インフラ由来
/// (= 503)」を切り分けるための型付きエラー。CLI 層では `anyhow::Error` に
/// flatten するが、HTTP 層 (`local_api::follow_request`) は variant を見て
/// status code を出し分ける。
#[derive(Debug, Error)]
pub enum MutateError {
    /// 指定 `follow.id` の行が存在しない (= 既に削除済み or 誤った id)。
    #[error("no follow row with id={0}")]
    NotFound(i64),
    /// 行は存在するが `pending` ではない (= 既に approve/reject 済みか、
    /// 同時実行で他のセッションに先取りされた)。
    #[error("follow id={id} is in state {state}; only `pending` rows can be approved/rejected")]
    NotPending { id: i64, state: String },
    /// 行の `followed` が remote actor (= 我々が approve/reject する立場に
    /// ない)。通常は dispatch 経路で弾かれているため到達しないが、
    /// DB を直接弄ったケースのための明示ガード。
    #[error("follow id={id} targets remote actor {ap_id}; refusing to mutate from this side")]
    NotForLocal { id: i64, ap_id: String },
    /// DB / 配送先 URL parse / serialize 等のインフラ層失敗。HTTP では 503。
    #[error(transparent)]
    Infra(anyhow::Error),
}

impl From<sqlx::Error> for MutateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Infra(e.into())
    }
}

pub async fn run(config: Config, args: FollowRequestArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        FollowRequestCommand::List => list(&state).await,
        FollowRequestCommand::Approve(a) => mutate_print(&state, a.id, ResponseType::Accept).await,
        FollowRequestCommand::Reject(a) => mutate_print(&state, a.id, ResponseType::Reject).await,
    }
}

/// `follow_id` の pending Follow を `new_state` (Accepted / Rejected) に倒し、
/// 対応する Accept / Reject activity を `delivery_queue` に積む。
///
/// CLI 実装と統合テスト / local API の共通エントリ。テストは
/// `AppState::from_pool` で生成した state を渡せる。
///
/// `FollowState::Pending` を渡すのは内部誤用 ── `MutateError::Infra` で
/// 弾く (= caller の入口で Accept / Reject の二択に絞る設計)。
pub async fn approve_or_reject(
    state: &AppState,
    follow_id: i64,
    new_state: FollowState,
) -> Result<(), MutateError> {
    let response_type = match new_state {
        FollowState::Accepted => ResponseType::Accept,
        FollowState::Rejected => ResponseType::Reject,
        FollowState::Pending => {
            return Err(MutateError::Infra(anyhow::anyhow!(
                "approve_or_reject called with FollowState::Pending (internal misuse)"
            )));
        }
    };
    mutate(state, follow_id, response_type).await.map(|_| ())
}

async fn list(state: &AppState) -> anyhow::Result<()> {
    let rows = repo::follow::list_pending_for_local(state.pool())
        .await
        .context("list pending follow requests")?;
    if rows.is_empty() {
        println!("(no pending follow requests)");
        return Ok(());
    }
    println!("ID\tRECEIVED (UTC)\tFOLLOWER\tFOLLOW_AP_ID");
    for (id, ap_id, follower_ap_id, created_at) in rows {
        println!(
            "{id}\t{ts}\t{follower}\t{ap_id}",
            ts = created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            follower = follower_ap_id,
        );
    }
    // **PR #80 round-2 review #9 (informational)**: 鍵アカ中に approve せず
    // 放置すると、相手 (Mastodon) は最大 ~7 日リトライしたあと諦めて Accept
    // を無視する。管理者が UI で気付けるよう foot note を出す。
    println!();
    println!(
        "(NOTE: remote servers retry inbound Follow for ~7 days. Approve/reject \
         requests within that window to avoid the remote giving up.)"
    );
    Ok(())
}

/// CLI から呼ばれる薄いラッパ。エラーを `anyhow` に flatten して
/// 通常の bail message として出力する。
async fn mutate_print(
    state: &AppState,
    follow_id: i64,
    response_type: ResponseType,
) -> anyhow::Result<()> {
    let (row, follower, queued_id, inbox_url) = mutate(state, follow_id, response_type)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    // `mutate` 内の `wake_delivery` は daemon の local API 経路用。CLI は別
    // プロセスで daemon ワーカを起床できないので、積んだ Accept/Reject を
    // 自プロセスで即 flush して送る (#211 cross-process 回帰対応)。
    delivery::flush_due_now(state).await;
    println!(
        "follow-request {verb}: follow_id={id} queue_id={qid} follower={follower} inbox={inbox}",
        verb = response_type.verb(),
        id = row.id,
        qid = queued_id,
        follower = follower.ap_id,
        inbox = inbox_url,
    );
    Ok(())
}

/// 共通 mutate 実装。戻り値は `(row, follower, queue_id, inbox_url)`。
///
/// **PR #80 round-2 #1 atomic CAS 修正**:
/// - `set_state_if_pending` + `enqueue_activity` を **1 つの `Transaction`** で
///   囲み、どちらかが失敗したら全体を rollback する。
/// - CAS 表現 (`WHERE state = 'pending'`) によって TOCTOU を排除 ── 並列
///   approve が両方とも pending ガードを通って 2 本 enqueue する事故を防ぐ。
///   `rows_affected = 0` のとき `NotPending` (= 既に他のセッションが処理済み)
///   として 400 を返す。
async fn mutate(
    state: &AppState,
    follow_id: i64,
    response_type: ResponseType,
) -> Result<(FollowRow, ActorRow, i64, String), MutateError> {
    let row = repo::follow::get_by_id(state.pool(), follow_id)
        .await?
        .ok_or(MutateError::NotFound(follow_id))?;

    if row.state != FollowState::Pending.as_str() {
        return Err(MutateError::NotPending {
            id: row.id,
            state: row.state.clone(),
        });
    }

    let follower = repo::actor::get_by_id(state.pool(), row.follower_actor_id)
        .await?
        .ok_or_else(|| {
            MutateError::Infra(anyhow::anyhow!(
                "follow row id={} references missing follower actor",
                row.id
            ))
        })?;
    let followed = repo::actor::get_by_id(state.pool(), row.followed_actor_id)
        .await?
        .ok_or_else(|| {
            MutateError::Infra(anyhow::anyhow!(
                "follow row id={} references missing followed actor",
                row.id
            ))
        })?;

    if !followed.is_local {
        return Err(MutateError::NotForLocal {
            id: row.id,
            ap_id: followed.ap_id,
        });
    }

    let original_follow = build_original_follow(&row, &follower, &followed);
    let response =
        build_response_activity(state, &followed, &original_follow, row.id, response_type);
    let inbox_url = follower
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&follower.inbox_url)
        .to_string();

    let new_state = match response_type {
        ResponseType::Accept => FollowState::Accepted,
        ResponseType::Reject => FollowState::Rejected,
    };

    // ── atomic: CAS UPDATE → enqueue → commit ─────────────────────────────
    let mut tx = state
        .pool()
        .begin()
        .await
        .map_err(|e| MutateError::Infra(e.into()))?;

    let affected = repo::follow::set_state_if_pending(&mut *tx, row.id, new_state)
        .await
        .map_err(|e| MutateError::Infra(e.into()))?;
    if affected == 0 {
        // 別セッションが先に倒した。Re-read で現状を返して明示 NotPending
        // を表に出す ── 戻りに新しい state を入れたいので fetch し直す。
        let now_row = repo::follow::get_by_id(&mut *tx, row.id)
            .await
            .map_err(|e| MutateError::Infra(e.into()))?;
        let state_str = now_row.map_or_else(|| "<deleted>".into(), |r| r.state);
        return Err(MutateError::NotPending {
            id: row.id,
            state: state_str,
        });
    }

    let queued = delivery::enqueue_activity(&mut *tx, followed.id, &inbox_url, &response)
        .await
        .map_err(MutateError::Infra)?;

    tx.commit()
        .await
        .map_err(|e| MutateError::Infra(e.into()))?;
    // commit 後に wake (tx 内 enqueue 行は commit まで他コネクションに見えない)。
    state.wake_delivery();

    info!(
        follow_id = row.id,
        queue_id = queued.id,
        follower = %follower.ap_id,
        followed = %followed.ap_id,
        new_state = new_state.as_str(),
        "follow-request mutated",
    );
    Ok((row, follower, queued.id, inbox_url))
}

/// `(follow.ap_id, follower, followed)` から最小の Follow JSON を再構成する。
/// 元の activity 本文は DB に持っていないため、`id` / `type` / `actor` /
/// `object` だけの薄い object で Accept/Reject の `object` に詰める。
fn build_original_follow(row: &FollowRow, follower: &ActorRow, followed: &ActorRow) -> JsonValue {
    json!({
        "id": row.ap_id,
        "type": "Follow",
        "actor": follower.ap_id,
        "object": followed.ap_id,
    })
}

/// Accept / Reject activity を組み立てる。決定論的 ID で同じ follow row へ
/// の再叩き (= CLI 多重起動) を idempotent にする。
///
/// **PR #80 round-2 #5 対応**: `response_type` は文字列ではなく
/// [`ResponseType`] enum で受ける ── 将来の追加 (`Tentative` 等) で
/// `panic!` 経路に滑り込まないよう型システムで二択に固定する。
fn build_response_activity(
    state: &AppState,
    local_actor: &ActorRow,
    original_follow: &JsonValue,
    follow_id: i64,
    response_type: ResponseType,
) -> JsonValue {
    let activity_id = format!(
        "https://{host}/users/{user}/activities/{suffix}-{follow_id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
        suffix = response_type.id_suffix(),
    );
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": response_type.as_type(),
        "actor": local_actor.ap_id,
        "object": original_follow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::types::Json;

    fn fake_actor(id: i64, ap_id: &str) -> ActorRow {
        ActorRow {
            id,
            ap_id: ap_id.into(),
            preferred_username: "x".into(),
            host: "x.test".into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: format!("{ap_id}/inbox"),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: format!("{ap_id}#main-key"),
            public_key_pem: "PEM".into(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: Json(vec![]),
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".into(),
            manually_approves_followers: false,
            birthday: None,
            location: None,
            lang: None,
            followed_message: None,
            fields: Json(vec![]),
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn fake_follow(id: i64) -> FollowRow {
        FollowRow {
            id,
            ap_id: format!("https://remote.test/users/bob/follows/{id}"),
            follower_actor_id: 2,
            followed_actor_id: 1,
            state: "pending".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn original_follow_has_required_fields() {
        let row = fake_follow(7);
        let follower = fake_actor(2, "https://remote.test/users/bob");
        let followed = fake_actor(1, "https://x.test/users/alice");
        let v = build_original_follow(&row, &follower, &followed);
        assert_eq!(v["id"], "https://remote.test/users/bob/follows/7");
        assert_eq!(v["type"], "Follow");
        assert_eq!(v["actor"], "https://remote.test/users/bob");
        assert_eq!(v["object"], "https://x.test/users/alice");
    }
}
