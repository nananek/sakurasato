//! `Move` (= 引っ越し) activity の受領処理 (M9)。
//!
//! `ActivityStreams` の `Move` 仕様 (FEP-7628 / Mastodon 実装慣習):
//!
//! ```jsonc
//! {
//!   "@context": "https://www.w3.org/ns/activitystreams",
//!   "id": "https://old.example/users/alice/activities/move-1",
//!   "type": "Move",
//!   "actor": "https://old.example/users/alice",
//!   "object": "https://old.example/users/alice",   // 移動元 (= actor 本人)
//!   "target": "https://new.example/users/alice"     // 移動先
//! }
//! ```
//!
//! 移動が成立する条件 (= 「双方向の同意」検査):
//! 1. signer (= activity.actor) は body の `actor` と一致 (F3 で確認済み)
//! 2. activity.`object` == `signer.ap_id` (= 「自分が動いた」と宣言できるのは本人)
//! 3. activity.`target` は別 URI で、その actor の `alsoKnownAs` に `object` が
//!    含まれていなければならない (= 受け入れ側の同意)
//!
//! これを満たしたら:
//! - 移動元 actor (= signer) の `moved_to_ap_id` を target に倒す ── 以後の actor
//!   JSON で `movedTo` を返し、相手側が再 fetch すれば自然に新先へ案内される。
//! - **我々の local actor が signer を follow している** 場合は、自動的に target
//!   への新規 Follow を `delivery_queue` に積む ── これが「フォロワー引き継ぎ」
//!   のお一人様サーバ側の実装 (CLAUDE.md §13)。
//!
//! ## 非実装: フォロワー側の Undo Follow
//!
//! Mastodon は Move を送るとき同時に各フォロワーに `Undo Follow` も送るが、
//! sakurasato は signer 側 Move を **受領した側** にすぎないので、自身が
//! 元 actor を unfollow するかどうかは判断材料に乏しい (= 双方残し / 強制
//! 解除どちらも選べる)。当面は **両方残す** ── 元 actor の `movedTo` が立つ
//! 以上、相手の判断で配送が止まる構造にする。

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::{Value as JsonValue, json};
use tracing::{info, warn};

use crate::delivery;
use crate::remote_actor;
use crate::state::AppState;

/// Move activity の受領処理。
///
/// 失敗時に `Err` で返したものは [`super::dispatch`] が 503 で返す
/// (= Mastodon が retry を持つ)。`Malformed` 系は `super::DispatchError` で
/// 個別に返したいが、現状の `handle_*` 関数の戻り型に合わせて anyhow で返す
/// (= dispatch 内で 503 にラップ)。
pub(crate) async fn handle_move(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    // object: 移動元 = signer 本人であること。
    let object_uri = super::extract_object_uri(activity)
        .map_err(|e| anyhow!("Move has no usable `object`: {e}"))?
        .to_string();
    if object_uri != signer.ap_id {
        bail!(
            "Move `object` {object_uri:?} does not match signer ap_id {:?}; refusing",
            signer.ap_id,
        );
    }

    // target: 移動先 URI。string または `{id: ...}` object。
    let target_uri = extract_target_uri(activity)
        .ok_or_else(|| anyhow!("Move has no usable `target` field"))?
        .to_string();
    if target_uri == object_uri {
        bail!("Move `target` equals `object`; nothing to migrate");
    }

    // 移動先 actor を **本番では常に fresh で取り直す** ([[m9-pr1-review]] [1]
    // 対応)。DB キャッシュをそのまま使うと、target が一度 `alsoKnownAs = [source]`
    // で取り込まれた後に同意を撤回 (= `alsoKnownAs` から source を消した)
    // ケースで、古い同意フラグを元に Move を受理してしまう。Actor JSON 取得は
    // 画像デコードを伴わない (= JSON のみ) ので、`remote_actor::fetch_and_upsert`
    // を経由しても CLAUDE.md §3 の隔離設計 (= 信頼できないバイト列の decode は
    // media-proxy 側) に抵触しない。SSRF / redirect / size 上限はその中で
    // 既存ガードを通る。
    //
    // テスト経路 (`AppState::from_pool` で `enable_remote_fetch=false`) では
    // DB に予め seed された target を使う ── `extract.rs` の未知 actor 処理
    // と同じ規約 (= "テストは必要な actor を予め `repo::actor::insert` で
    // seed しておく契約")。
    let target = ensure_target_actor(state, &target_uri).await?;

    // 双方向同意検査: target.alsoKnownAs に object (= 移動元) が含まれていない
    // とダメ。これが無いと「A の Move を勝手に偽装」が成立してしまう。
    if !target.also_known_as.0.iter().any(|a| a == &object_uri) {
        bail!(
            "Move target {target_uri:?} does not list source {object_uri:?} in alsoKnownAs; refusing",
        );
    }

    // 元 actor (= signer) に movedTo を立てる。これは情報的なフラグであり、
    // 既存 follow 行を消すことはしない (Mastodon と同じ)。
    let updated_source = repo::actor::set_moved_to(state.pool(), signer.id, Some(&target.ap_id))
        .await
        .context("set moved_to on source actor")?;
    info!(
        source = %updated_source.ap_id,
        target = %target.ap_id,
        "Move accepted; source actor marked as moved",
    );

    // 我々の local actor が signer を follow していたら、target への
    // Follow を自動で積む (フォロワー引き継ぎ)。お一人様サーバなので
    // 該当する local follower はせいぜい 1 人。
    let movers = repo::follow::list_local_following(state.pool(), signer.id)
        .await
        .context("list local followers of moved actor")?;
    for (old_follow_id, local_id) in movers {
        if let Err(err) = enqueue_auto_refollow(state, local_id, &target, old_follow_id).await {
            // 1 件失敗しても残りは続行 (再 Move には対応しないが、CLI で手で
            // 投げ直すこともできる)。
            warn!(
                ?err,
                local_id,
                target = %target.ap_id,
                "auto re-follow after Move failed; continuing",
            );
        }
    }

    Ok(())
}

/// `Move.target` から URI を取り出す。`object` と同じく文字列 / `{id: ...}` 双方を受ける。
fn extract_target_uri(activity: &JsonValue) -> Option<&str> {
    match activity.get("target")? {
        JsonValue::String(s) => Some(s.as_str()),
        JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str),
        _ => None,
    }
}

/// Move target の actor row を確保する。
///
/// 本番経路 (= `enable_remote_fetch=true`) では **常に fresh で fetch**
/// する ── キャッシュ済みの古い `alsoKnownAs` で同意検査をやってしまうと、
/// 同意撤回後の偽 Move を受理してしまうため ([[m9-pr1-review]] [1])。
/// テスト経路 (`enable_remote_fetch=false`) では DB seed を使う。
async fn ensure_target_actor(state: &AppState, ap_id: &str) -> anyhow::Result<ActorRow> {
    if state.enable_remote_fetch() {
        return remote_actor::fetch_and_upsert(state, ap_id)
            .await
            .with_context(|| format!("fetch Move target actor {ap_id}"));
    }
    repo::actor::get_by_ap_id(state.pool(), ap_id)
        .await
        .with_context(|| format!("lookup Move target {ap_id} in DB"))?
        .ok_or_else(|| {
            anyhow!(
                "Move target {ap_id} not in DB and remote fetch is disabled (test mode); \
                 seed the target via repo::actor::insert first",
            )
        })
}

/// 自分の local actor が移動元 actor を follow していたとき、target へ
/// 新しい Follow を積む。
///
/// 失敗 (重複 / DB エラー等) は呼び出し側で warn のみ。
async fn enqueue_auto_refollow(
    state: &AppState,
    local_actor_id: i64,
    target: &ActorRow,
    old_follow_id: i64,
) -> anyhow::Result<()> {
    let local = repo::actor::get_by_id(state.pool(), local_actor_id)
        .await
        .context("lookup local follower actor")?
        .ok_or_else(|| anyhow!("local follower {local_actor_id} disappeared"))?;
    if !local.is_local {
        // 防御的: list_local_following が is_local = true で絞っているので
        // ここに来ないはずだが、念のため。
        bail!(
            "follower {} is not local; refusing to send Follow",
            local.ap_id
        );
    }

    // 既に target を follow していれば何もしない。お一人様サーバなので
    // 重複 Follow を送ると相手の inbox を汚す。
    if (repo::follow::list_local_following(state.pool(), target.id).await?)
        .iter()
        .any(|(_, follower_id)| *follower_id == local.id)
    {
        info!(
            local = %local.ap_id,
            target = %target.ap_id,
            "already following Move target; skipping auto re-follow",
        );
        return Ok(());
    }

    // Follow activity ID は決定論: 元 Follow row id を含めることで、後で
    // 「どの Move から派生したか」が追える + 重複時の冪等性が高い。
    let follow_ap_id = format!(
        "https://{host}/users/{user}/activities/follow-move-{old_follow_id}",
        host = state.config().server.host,
        user = local.preferred_username,
    );

    // DB 側にも pending follow を作る (Accept を待つ)。`(follower, followed)`
    // UNIQUE のため、既存があれば `upsert_pending` が既存を返す。
    let row = repo::follow::upsert_pending(state.pool(), &follow_ap_id, local.id, target.id)
        .await
        .context("upsert pending follow for Move target")?;

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_ap_id,
        "type": "Follow",
        "actor": local.ap_id,
        "object": target.ap_id,
    });

    let inbox = target
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target.inbox_url);
    delivery::enqueue_activity(state.pool(), local.id, inbox, &activity)
        .await
        .with_context(|| format!("enqueue auto-Follow to {inbox}"))?;
    info!(
        local = %local.ap_id,
        target = %target.ap_id,
        follow_id = row.id,
        "auto-Follow queued after Move",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_target_uri_accepts_string_and_object() {
        let s = json!({"target": "https://x.test/u/a"});
        assert_eq!(extract_target_uri(&s), Some("https://x.test/u/a"));
        let o = json!({"target": {"id": "https://x.test/u/b", "type": "Person"}});
        assert_eq!(extract_target_uri(&o), Some("https://x.test/u/b"));
        // 欠落 / 非 URI は None。
        let missing = json!({});
        assert_eq!(extract_target_uri(&missing), None);
        let null = json!({"target": null});
        assert_eq!(extract_target_uri(&null), None);
        let arr = json!({"target": [42]});
        assert_eq!(extract_target_uri(&arr), None);
    }
}
