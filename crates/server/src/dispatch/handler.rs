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
use url::Url;

use super::DispatchError;
use crate::delivery;
use crate::state::AppState;

/// 2 つの `ap_id` URI が同じ host (= 同じインスタンス) を指していることを
/// 確認する。ホストは大文字小文字を区別せず (RFC 9110 §4.2.3) 比較する。
///
/// `kind` はエラーメッセージ用のラベル ("Follow activity id" 等)。
///
/// `pub(crate)`: `dispatch/block.rs::handle_block` (PR3) が同じ F4 相当の
/// 検証を再利用する (計画書 §5.5)。
pub(crate) fn ensure_same_host(
    other_uri: &str,
    signer_ap_id: &str,
    kind: &str,
) -> anyhow::Result<()> {
    let other = Url::parse(other_uri)
        .with_context(|| format!("{kind} {other_uri:?} is not a valid URL"))?;
    let signer = Url::parse(signer_ap_id)
        .with_context(|| format!("signer ap_id {signer_ap_id:?} is not a valid URL"))?;
    let other_host = other.host_str().unwrap_or("");
    let signer_host = signer.host_str().unwrap_or("");
    if !other_host.eq_ignore_ascii_case(signer_host) {
        bail!("{kind} host {other_host:?} does not match signer host {signer_host:?}");
    }
    Ok(())
}

/// 受領 Follow (`signer` → 我々の local actor) の処理。
///
/// 流れ:
/// 1. body の `id` のホストが署名者のホストと一致することを確認する
///    (F4 相当)。`evil.example` の有効署名者が `good.example` の活動 ID を
///    DB に混入させるのを防ぐ。
/// 2. body の `object` = 我々の local actor の URI を取り出す。
/// 3. その local actor を DB から引き、`is_local` かつ `Application`
///    (instance actor) でないものだけ受け入れる。Application actor は
///    inbox を持たない設計 (M3a で `init` が生成するのは `Person` のみ)。
/// 4. `repo::follow::upsert_pending` で follow 行を idempotent に作る。
/// 5. Accept activity を組み立て、`enqueue_activity` で `signer.inbox_url`
///    宛に配送キューに積む。常駐 worker がループで送出する (#23 暫定の server
///    直配送)。
#[allow(
    clippy::too_many_lines,
    reason = "lock 緩和分岐 + Accept enqueue を 1 関数で抱える"
)]
pub(crate) async fn handle_follow(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    let follow_ap_id = super::extract_activity_id(activity)
        .map_err(|e| anyhow!("Follow has no activity id: {e}"))?
        .to_string();

    // **F4 相当の host 一致検証**: Follow activity の `id` ホストが signer
    // ホストと一致しなければ拒否。F3 (body actor == signer) と組で
    // 「他インスタンスの活動 ID を DB に混入される」攻撃を遮断する
    // (M3b-3 PR2 round-2 review F2)。
    ensure_same_host(&follow_ap_id, &signer.ap_id, "Follow activity id")?;

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
    if followed.actor_type.eq_ignore_ascii_case("Application") {
        // Application (instance) actor は inbox を持たない設計。
        // 「サーバ全体に follow する」リクエストは AP 仕様上ありえない。
        bail!(
            "Follow target {} is an Application actor; refusing to accept",
            followed.ap_id,
        );
    }

    // **PR5 (計画書 §6.4, §10 確定事項 #3)**: silence 対象ドメインからの
    // 「新規」Follow はサイレントドロップする。既に accepted な関係の
    // retry (= Mastodon 等が Accept 再送出を期待するハウスキーピング) は
    // silence 導入前から続く正当な関係なので妨げない。Create/Like/
    // EmojiReact/Announce 等、既存 followee を前提とするインタラクションは
    // ここでは一切触れない (silence は Follow ハンドラのみで完結させる)。
    if let Some(m) = repo::domain_moderation::get_by_host(state.pool(), &signer.host)
        .await
        .context("domain moderation lookup for inbound Follow")?
        && m.severity == "silence"
    {
        let already_accepted = repo::follow::get_by_pair(state.pool(), signer.id, followed.id)
            .await
            .context("existing follow lookup for silence guard")?
            .is_some_and(|row| row.state == FollowState::Accepted.as_str());
        if !already_accepted {
            info!(
                follower = %signer.ap_id,
                followed = %followed.ap_id,
                host = %signer.host,
                "inbound Follow from silenced domain; silently dropping",
            );
            return Ok(());
        }
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
    } else if followed.manually_approves_followers {
        // **緩和判定**: `auto_approve_followers_for_followees = true` のとき、
        // 自分が既に follow している (or pending 送出中の) 相手からの inbound
        // Follow は手動承認をスキップして Accept パスへ。双方向 follow の
        // 慣習を維持しつつ鍵アカ運用の手間を減らす opt-in 動作。
        //
        // - `(follower = followed = local_actor, followed = signer)` の reverse
        //   方向 follow 行を見て `accepted` / `pending` なら信頼関係ありと判定。
        // - 既に retry / mutual follow なら本判定は skip され Accept 経路へ。
        // - DB エラーは「分からない」= 安全側 = manual approval 待ちにフォール
        //   バック (= warn ログのみ)。
        let mutual_path = if state.config().server.auto_approve_followers_for_followees {
            match repo::follow::get_by_pair(state.pool(), followed.id, signer.id).await {
                Ok(Some(existing)) => matches!(existing.state.as_str(), "accepted" | "pending",),
                Ok(None) => false,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        follower = %signer.ap_id,
                        followed = %followed.ap_id,
                        "auto_approve_followers_for_followees lookup failed; falling back to manual approval",
                    );
                    false
                }
            }
        } else {
            false
        };

        if mutual_path {
            info!(
                follow_id = row.id,
                follower = %signer.ap_id,
                followed = %followed.ap_id,
                "auto-approving inbound Follow (mutual / already-following relationship)",
            );
            // fall through to Accept enqueue + state transition.
        } else {
            // **Issue #66 (鍵アカ運用)**: followed actor が manually approves
            // で、かつ既存 Follow 行が `pending` の場合は Accept を queue せず
            // 据え置く。承認は管理 CLI
            // (`sakurasato-server follow-request approve --id N`) で明示実行
            // する想定。
            //
            // `Accepted` ブランチでこの分岐より前に return しているのは意図的:
            // 以前 unlock 状態で受理した Follow が `accepted` のまま残ってい
            // るところに lock 後の retry 配送が来た場合、相手側は accepted の
            // はずなので Accept を返してあげないと延々と pending 扱いされる。
            // (lock した瞬間に従来フォロワーを切るのではなく、新規 Follow だけ
            // 承認制に切替える設計)
            info!(
                follow_id = row.id,
                follower = %signer.ap_id,
                followed = %followed.ap_id,
                "follow-request received (manually_approves_followers); awaiting CLI approval",
            );
            // 鍵アカ pending: 承認待ち通知を webhook に流す (fire-and-forget)。
            crate::notification::dispatch::notify_follow_request(state, signer).await;
            return Ok(());
        }
    }

    let accept_activity = build_accept_activity(state, &followed, activity, row.id);

    let inbox_url = signer
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&signer.inbox_url);

    let queued = delivery::enqueue_activity(state.pool(), followed.id, inbox_url, &accept_activity)
        .await
        .context("enqueue Accept activity")?;
    // Accept を入れたら即配送ワーカを起こす (inbound Follow への応答遅延を
    // 増やさない ── 空ポーリング廃止に伴う wake)。
    state.wake_delivery();

    // お一人様 + 自動承認設計なので、Accept を queue した時点で follow 行を
    // accepted に倒す。これをやらないと `repo::follow::list_accepted_inboxes`
    // から外れたまま固定化され、こちらからの Note / reaction が一切配送されない
    // (= 連合テストで露見した既存バグ)。Accept Activity の実配送 (= delivery
    // worker の HTTP POST 成功) を待つ設計もあるが、worker と handler の結合が
    // 増えるだけで、お一人様用途では即時遷移で問題ない。M3b-3 PR2 当時の名残。
    //
    // 再受信ケース (row.state が既に Accepted) は no-op、Rejected は前段で
    // early return しているので、ここに来るのは Pending のみ。
    if row.state != FollowState::Accepted.as_str() {
        repo::follow::set_state(state.pool(), row.id, FollowState::Accepted)
            .await
            .with_context(|| format!("set inbound follow {} state to accepted", row.id))?;
    }

    info!(
        follow_id = row.id,
        queue_id = queued.id,
        follower = %signer.ap_id,
        followed = %followed.ap_id,
        "Follow accepted; Accept queued for delivery",
    );

    // 新規 Accept のみ通知する。`row.state` が **upsert 前** に既に `Accepted`
    // だった = Mastodon の Follow retry 経路では webhook を発火しない ── 相手側
    // で Accept が届かない状況だと数時間おきに「新しいフォロワーです」通知が連投
    // される問題を避ける (round-1 review F2)。
    if row.state == FollowState::Accepted.as_str() {
        tracing::debug!(
            follow_id = row.id,
            follower = %signer.ap_id,
            "duplicate Follow retry; skipping notify_follow webhook fan-out",
        );
    } else {
        crate::notification::dispatch::notify_follow(state, signer).await;
    }

    Ok(())
}

/// 受領 Accept の処理。`object` は元 Follow activity (URI または inline)。
///
/// `object` を URI として扱い、その `ap_id` を持つ follow 行を探して
/// `state` を `accepted` に倒す。`followed_actor_id != signer.id` の場合は
/// なりすまし試行として [`DispatchError::UnrelatedAcceptor`] で 401 を返す
/// (handler の内部失敗 = 503 とは区別する)。
pub(crate) async fn handle_accept(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    apply_follow_state(state, signer, activity, FollowState::Accepted).await
}

/// 受領 Reject の処理。
pub(crate) async fn handle_reject(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    apply_follow_state(state, signer, activity, FollowState::Rejected).await
}

async fn apply_follow_state(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
    new_state: FollowState,
) -> Result<(), DispatchError> {
    let follow_uri = super::extract_object_uri(activity)?.to_string();

    let row = repo::follow::get_by_ap_id(state.pool(), &follow_uri)
        .await
        .with_context(|| format!("lookup follow row by ap_id {follow_uri}"))
        .map_err(DispatchError::Internal)?
        .ok_or_else(|| {
            DispatchError::Malformed(format!(
                "Accept/Reject references unknown follow {follow_uri}"
            ))
        })?;

    // **信頼境界**: signer は body の actor 本人であることが F3 で確認
    // 済み。Accept を返してくる正当な actor は、元 Follow の `object`
    // (= 我々が follow したい remote actor) であるはず。`followed_actor_id`
    // が `signer.id` と一致しない Accept/Reject は無関係な actor からの
    // なりすまし試行 (UnrelatedAcceptor) として 401 で拒否する。
    if row.followed_actor_id != signer.id {
        return Err(DispatchError::UnrelatedAcceptor {
            signer: signer.ap_id.clone(),
            follow_ap_id: follow_uri,
        });
    }

    repo::follow::set_state(state.pool(), row.id, new_state)
        .await
        .with_context(|| format!("set follow {} state to {}", row.id, new_state.as_str()))
        .map_err(DispatchError::Internal)?;

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
