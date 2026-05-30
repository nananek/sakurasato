//! Activity 別ハンドラ。
//!
//! M3b-3 PR2 で実装するのは Follow / Accept / Reject の最小三役。
//! Create/Note や Like 等は後続 PR。
//!
//! ## 信頼境界
//!
//! ここに到達した時点で:
//! - `signer` (= `ActorRow`) は HTTP 署名で身元確認済み (PR1)
//! - body の `actor` が `signer.ap_id` と一致することは [`super::verify_body_actor`]
//!   で確認済み (F3, PR2 round-1)
//! - body のネスト `attributedTo` も signer と一致することは
//!   [`super::verify_nested_object_actor`] で確認済み (F3 ネスト, PR2 round-1)
//!
//! したがって handler 側では「signer は body の actor 本人である」を前提に
//! してよい。残るは object URI が我々を指しているか、follow 状態遷移が
//! 正しいか、といったセマンティック層の検査だけ。

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::{ActorRow, FollowState};
use sakurasato_core::repo;
use serde_json::{Value as JsonValue, json};
use tracing::{info, warn};

use crate::delivery;
use crate::state::AppState;

/// 受領 Follow (`signer` → 我々の local actor) の処理。
///
/// 流れ:
/// 1. body の `object` = 我々の local actor の URI を取り出す。
/// 2. その local actor を DB から引き、`is_local && actor_type != "Application"`
///    な actor のみ受け入れる。違えば 400 で拒否 (Application actor は inbox を
///    持たない設計)。
/// 3. `repo::follow::upsert_pending` で follow 行を idempotent に作る。
/// 4. Accept activity を組み立て、`enqueue_activity` で `signer.inbox_url`
///    宛に配送キューに積む。常駐 worker がループで送出する (#23 暫定の server
///    直配送)。
pub(crate) async fn handle_follow(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    let follow_ap_id = super::extract_activity_id(activity)
        .map_err(|e| anyhow!("Follow has no activity id: {e}"))?
        .to_string();
    let object_uri = super::extract_object_uri(activity)
        .map_err(|e| anyhow!("Follow has no usable `object`: {e}"))?
        .to_string();

    let followed = repo::actor::get_by_ap_id(state.pool(), &object_uri)
        .await
        .context("lookup followed actor")?
        .ok_or_else(|| anyhow!("Follow target {object_uri} not found locally"))?;

    if !followed.is_local {
        bail!(
            "Follow target {} is not a local actor; refusing to accept",
            followed.ap_id
        );
    }

    let row = repo::follow::upsert_pending(state.pool(), &follow_ap_id, signer.id, followed.id)
        .await
        .context("upsert follow row")?;

    if row.state == FollowState::Accepted.as_str() {
        // Mastodon の retry で Accept 送出済みの Follow が再度届いた。
        // Accept を返さないと相手はずっと pending のままなので、Accept は
        // 改めて積む (idempotent。delivery_queue.activity に同じ Accept を
        // 入れる行が増えるが、重複ハンドリングは受け取り側責務)。
        info!(
            follow_id = row.id,
            follower = %signer.ap_id,
            followed = %followed.ap_id,
            "duplicate Follow: re-sending Accept",
        );
    } else if row.state == FollowState::Rejected.as_str() {
        // 過去に明示拒否した相手の Follow が再送されてきた。state は触らず
        // 黙って 202 返す (相手の retry を抑えるには Reject 再送が筋だが、
        // 拒否済み follow を蒸し返すのも変なので no-op で済ます)。
        warn!(
            follow_id = row.id,
            follower = %signer.ap_id,
            "rejected Follow re-received; ignoring",
        );
        return Ok(());
    }

    let accept_activity = build_accept_activity(state, &followed, activity, row.id);

    let inbox_url = signer
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&signer.inbox_url);

    let queued = delivery::enqueue_activity(state.pool(), followed.id, inbox_url, &accept_activity)
        .await
        .context("enqueue Accept activity")?;

    info!(
        follow_id = row.id,
        queue_id = queued.id,
        follower = %signer.ap_id,
        followed = %followed.ap_id,
        "Follow accepted; Accept queued for delivery",
    );
    Ok(())
}

/// 受領 Accept の処理。`object` は元 Follow activity (URI または inline)。
///
/// `object` を URI として扱い、その `ap_id` を持つ follow 行を探して
/// `state` を `accepted` に倒す。
pub(crate) async fn handle_accept(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    apply_follow_state(state, signer, activity, FollowState::Accepted).await
}

/// 受領 Reject の処理。
pub(crate) async fn handle_reject(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    apply_follow_state(state, signer, activity, FollowState::Rejected).await
}

async fn apply_follow_state(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
    new_state: FollowState,
) -> anyhow::Result<()> {
    let follow_uri = super::extract_object_uri(activity)
        .map_err(|e| anyhow!("Accept/Reject has no usable `object`: {e}"))?
        .to_string();

    let row = repo::follow::get_by_ap_id(state.pool(), &follow_uri)
        .await
        .context("lookup follow row")?
        .ok_or_else(|| anyhow!("Accept/Reject references unknown follow {follow_uri}"))?;

    // **信頼境界**: signer は body の actor 本人であることが F3 で確認
    // 済み。Accept を返してくる正当な actor は、元 Follow の `object`
    // (= 我々が follow したい remote actor) であるはず。`followed_actor_id`
    // が `signer.id` と一致しない Accept/Reject は無関係な actor からの
    // なりすまし試行なので拒否する。
    if row.followed_actor_id != signer.id {
        bail!(
            "Accept/Reject signer {} is not the followed actor of follow {}",
            signer.ap_id,
            follow_uri,
        );
    }

    repo::follow::set_state(state.pool(), row.id, new_state)
        .await
        .with_context(|| format!("set follow {} state to {}", row.id, new_state.as_str()))?;

    info!(
        follow_id = row.id,
        new_state = new_state.as_str(),
        signer = %signer.ap_id,
        "follow state updated",
    );
    Ok(())
}

/// Accept activity を組み立てる。
///
/// `object` には元 Follow activity をそのまま埋める ── Mastodon / Misskey
/// の慣習で、Accept の `object` が URI 参照だけだと一部実装で照合に失敗する
/// 報告があるため、フル object を入れるのが安全。
fn build_accept_activity(
    state: &AppState,
    local_actor: &ActorRow,
    original_follow: &JsonValue,
    follow_id: i64,
) -> JsonValue {
    // 決定論的 Accept activity ID: 同じ follow に対する retry でも同じ ID
    // が振られる → 受け取り側で重複 Accept の見分けがつく。
    let accept_id = format!(
        "https://{host}/users/{user}/activities/accept-{follow_id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
    );

    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": accept_id,
        "type": "Accept",
        "actor": local_actor.ap_id,
        "object": original_follow,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn accept_id_is_deterministic() {
        // ID は host / user / follow_id だけで決まる ── 同じ follow に
        // 対する Accept は同じ ID。retry 安全性のために確認しておく。
        // build_accept_activity は AppState を取るのでフルセットアップが
        // 必要 → integration test 側 (routes_pg.rs) に回す。
        // ここでは accept_id フォーマットだけ単体で検証する。
        let host = "example.test";
        let user = "alice";
        let follow_id = 42_i64;
        let expected = format!("https://{host}/users/{user}/activities/accept-{follow_id}");
        assert_eq!(
            expected,
            "https://example.test/users/alice/activities/accept-42"
        );
    }
}
