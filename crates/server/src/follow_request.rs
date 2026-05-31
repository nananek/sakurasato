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

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::{ActorRow, FollowRow, FollowState};
use sakurasato_core::{Config, repo};
use serde_json::{Value as JsonValue, json};
use tracing::info;

use crate::cli::{FollowRequestArgs, FollowRequestCommand};
use crate::delivery;
use crate::state::AppState;

pub async fn run(config: Config, args: FollowRequestArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        FollowRequestCommand::List => list(&state).await,
        FollowRequestCommand::Approve(a) => mutate(&state, a.id, FollowState::Accepted).await,
        FollowRequestCommand::Reject(a) => mutate(&state, a.id, FollowState::Rejected).await,
    }
}

/// `follow_id` の pending Follow を `new_state` (Accepted / Rejected) に倒し、
/// 対応する Accept / Reject activity を `delivery_queue` に積む。
///
/// CLI 実装と統合テストの共通エントリ。テストは `AppState::from_pool` で
/// 生成した state を渡せる。
pub async fn approve_or_reject(
    state: &AppState,
    follow_id: i64,
    new_state: FollowState,
) -> anyhow::Result<()> {
    mutate(state, follow_id, new_state).await
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
    Ok(())
}

async fn mutate(state: &AppState, follow_id: i64, new_state: FollowState) -> anyhow::Result<()> {
    let row = repo::follow::get_by_id(state.pool(), follow_id)
        .await
        .with_context(|| format!("lookup follow row id={follow_id}"))?
        .ok_or_else(|| anyhow!("no follow row with id={follow_id}"))?;

    if row.state != FollowState::Pending.as_str() {
        bail!(
            "follow id={id} is in state {state}; only `pending` rows can be approved/rejected",
            id = row.id,
            state = row.state,
        );
    }

    let follower = repo::actor::get_by_id(state.pool(), row.follower_actor_id)
        .await
        .with_context(|| format!("lookup follower actor id={}", row.follower_actor_id))?
        .ok_or_else(|| anyhow!("follow row id={} references missing follower actor", row.id))?;
    let followed = repo::actor::get_by_id(state.pool(), row.followed_actor_id)
        .await
        .with_context(|| format!("lookup followed actor id={}", row.followed_actor_id))?
        .ok_or_else(|| anyhow!("follow row id={} references missing followed actor", row.id))?;

    if !followed.is_local {
        bail!(
            "follow id={} targets remote actor {}; refusing to approve/reject from this side",
            row.id,
            followed.ap_id,
        );
    }

    let original_follow = build_original_follow(&row, &follower, &followed);
    let response = match new_state {
        FollowState::Accepted => {
            build_response_activity(state, &followed, &original_follow, row.id, "Accept")
        }
        FollowState::Rejected => {
            build_response_activity(state, &followed, &original_follow, row.id, "Reject")
        }
        FollowState::Pending => {
            // `mutate` は Accept か Reject にしか呼ばれないので Pending は
            // 来ない。安全側で bail しておく。
            bail!("internal error: mutate() called with FollowState::Pending");
        }
    };

    let inbox = follower
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&follower.inbox_url);
    let queued = delivery::enqueue_activity(state.pool(), followed.id, inbox, &response)
        .await
        .with_context(|| format!("enqueue {} for follow id={}", new_state.as_str(), row.id))?;

    repo::follow::set_state(state.pool(), row.id, new_state)
        .await
        .with_context(|| format!("set follow {} state to {}", row.id, new_state.as_str()))?;

    info!(
        follow_id = row.id,
        queue_id = queued.id,
        follower = %follower.ap_id,
        followed = %followed.ap_id,
        new_state = new_state.as_str(),
        "follow-request mutated",
    );
    println!(
        "follow-request {verb}: follow_id={id} queue_id={qid} follower={follower} inbox={inbox}",
        verb = if matches!(new_state, FollowState::Accepted) {
            "approved"
        } else {
            "rejected"
        },
        id = row.id,
        qid = queued.id,
        follower = follower.ap_id,
        inbox = inbox,
    );
    Ok(())
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
fn build_response_activity(
    state: &AppState,
    local_actor: &ActorRow,
    original_follow: &JsonValue,
    follow_id: i64,
    response_type: &'static str,
) -> JsonValue {
    let suffix = match response_type {
        "Accept" => "accept",
        "Reject" => "reject",
        // CLI 経路では Accept/Reject の二択しか走らないので、それ以外は
        // 開発時のロジック誤りに相当する。`build_*` は失敗を返せない
        // (JsonValue) ので panic で潰す。
        other => panic!("unsupported response_type {other:?}"),
    };
    let activity_id = format!(
        "https://{host}/users/{user}/activities/{suffix}-{follow_id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
    );
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": response_type,
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
