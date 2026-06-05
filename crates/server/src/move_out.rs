//! `sakurasato alias …` と `sakurasato move-out …` — M9 引っ越し用 CLI。
//!
//! ## `alias`
//!
//! `alsoKnownAs` は「自分は過去にこの actor だった」と宣言するフィールド。
//! 移動 **先** の actor がこれを立てておかないと、移動 **元** が送る Move
//! を受領側が拒否する (= 双方向同意検査)。本 CLI は新ホストに引っ越して
//! 来た直後、`alias add https://old.example/users/me` を打って受け入れ準備を
//! 整える用途を主に想定している。
//!
//! 変更は **Update Activity** でフォロワーに配信される (M7 プロフィール変更と
//! 同じ流れ)。
//!
//! ## `move-out`
//!
//! sakurasato → 別ホストへ自分が移るとき用。`target` の `alsoKnownAs` に
//! 自分が含まれていることを `remote_actor::fetch_and_upsert` で取得 + 検証し、
//! `moved_to_ap_id` を立てる + フォロワー全員に `Move` 配信する。
//! お一人様サーバなので頻度は極めて低い (= 「サーバ捨てて他に移る」用途)。

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::ActorRow;
use sakurasato_core::{Config, repo};
use serde_json::{Value as JsonValue, json};
use tracing::{info, warn};
use url::Url;

use crate::cli::{AliasArgs, AliasCommand, AliasMutateArgs, MoveOutArgs};
use crate::delivery;
use crate::local_api::profile::build_update_activity;
use crate::remote_actor;
use crate::state::AppState;

const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";

/// `sakurasato alias <subcommand>` のエントリポイント。
pub async fn run_alias(config: Config, args: AliasArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let local = local_actor(&state).await?;

    match args.command {
        AliasCommand::List => {
            print_aliases(&local.also_known_as.0);
            Ok(())
        }
        AliasCommand::Add(AliasMutateArgs { uri }) => {
            // **URI バリデーション** ([[m9-pr1-review]] [2] 対応): 任意の
            // 文字列を `alsoKnownAs` に入れると、actor JSON 経由で連合先に
            // 投げたとき相手側のパーサが落ちる可能性がある (`javascript:` /
            // 空文字 / 制御文字混入など)。CLAUDE.md §5.1 が想定するのは
            // 「過去の actor の `ap_id`」のみなので、http/https のみを許可。
            validate_alias_uri(&uri)?;
            mutate_alias(&state, &local, |list| {
                if list.iter().any(|u| u == &uri) {
                    info!(uri = %uri, "alsoKnownAs already contains URI; no change");
                    false
                } else {
                    list.push(uri.clone());
                    true
                }
            })
            .await
        }
        AliasCommand::Remove(AliasMutateArgs { uri }) => {
            mutate_alias(&state, &local, |list| {
                let before = list.len();
                list.retain(|u| u != &uri);
                before != list.len()
            })
            .await
        }
        AliasCommand::Clear => {
            mutate_alias(&state, &local, |list| {
                let changed = !list.is_empty();
                list.clear();
                changed
            })
            .await
        }
    }
}

/// `sakurasato move-out <target>` のエントリポイント。
pub async fn run_move_out(config: Config, args: MoveOutArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let local = local_actor(&state).await?;
    if local.ap_id == args.target {
        bail!("`target` equals the local actor URI; nothing to migrate");
    }
    if local.moved_to_ap_id.as_deref() == Some(&args.target) {
        bail!(
            "local actor is already marked as moved to {}; nothing to do",
            args.target
        );
    }

    // target を fetch & DB upsert。これで alsoKnownAs と inbox URL が手に入る。
    let target = remote_actor::fetch_and_upsert(&state, &args.target)
        .await
        .with_context(|| format!("fetch move target actor {}", args.target))?;

    if args.force {
        warn!(
            target = %target.ap_id,
            "move-out --force: skipping bidirectional alsoKnownAs check",
        );
    } else if !target.also_known_as.0.iter().any(|a| a == &local.ap_id) {
        // 双方向同意: 移動先の alsoKnownAs に自分の ap_id が居なければダメ。
        bail!(
            "move target {target} does not list {local} in alsoKnownAs; \
             add it on the target side first or rerun with --force",
            target = target.ap_id,
            local = local.ap_id,
        );
    }

    // movedTo を立てる。actor JSON も以後これを含めて返す。
    let updated = repo::actor::set_moved_to(state.pool(), local.id, Some(&target.ap_id))
        .await
        .context("set moved_to on local actor")?;

    let move_activity = build_move_activity(&updated, &target.ap_id);
    let queued = enqueue_to_followers(&state, &updated, &move_activity).await;
    info!(
        target = %target.ap_id,
        queued,
        "Move activity queued for delivery to followers",
    );
    println!(
        "Move activity queued ({} follower inbox(es)). target = {}",
        queued, target.ap_id,
    );
    Ok(())
}

/// `alsoKnownAs` に入れる URI が `ActivityPub` actor URI として妥当か検査。
///
/// 受け入れ条件:
/// - `url::Url::parse` を通る
/// - scheme が `https` または `http`
/// - host が存在する (= 空ではない)
///
/// 拒否例: 空文字、`javascript:alert(1)`、`mailto:`、相対パス、制御文字混入。
fn validate_alias_uri(raw: &str) -> anyhow::Result<()> {
    let parsed = Url::parse(raw)
        .with_context(|| format!("invalid alias URI {raw:?}: not a parseable URL"))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        bail!(
            "invalid alias URI {raw:?}: scheme must be https or http, got {:?}",
            parsed.scheme(),
        );
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        bail!("invalid alias URI {raw:?}: host is missing");
    }
    Ok(())
}

async fn local_actor(state: &AppState) -> anyhow::Result<ActorRow> {
    let host = state.config().server.host.clone();
    let user = state.config().server.user.clone();
    let row = repo::actor::get_by_username_host(state.pool(), &user, &host)
        .await
        .context("lookup local actor")?
        .ok_or_else(|| {
            anyhow!("local actor {user}@{host} not initialised; run `sakurasato init`")
        })?;
    if !row.is_local {
        bail!("actor {user}@{host} exists but is not local (corrupted state?)");
    }
    Ok(row)
}

fn print_aliases(list: &[String]) {
    if list.is_empty() {
        println!("(no alsoKnownAs entries)");
        return;
    }
    for uri in list {
        println!("{uri}");
    }
}

async fn mutate_alias<F>(state: &AppState, local: &ActorRow, mutate: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Vec<String>) -> bool,
{
    let mut list = local.also_known_as.0.clone();
    let changed = mutate(&mut list);
    if !changed {
        println!("(no change)");
        return Ok(());
    }
    let updated = repo::actor::set_also_known_as(state.pool(), local.id, &list)
        .await
        .context("persist alsoKnownAs update")?;

    print_aliases(&updated.also_known_as.0);

    // Update activity でフォロワーに配信。M7 のプロフィール変更と同じ Update
    // を使い回す ── object に actor JSON 全体を載せるので `alsoKnownAs` も
    // そのまま伝わる。
    let activity = build_update_activity(&updated);
    let queued = enqueue_to_followers(state, &updated, &activity).await;
    eprintln!("(Update queued for {queued} follower inbox(es))");
    Ok(())
}

/// `Move` activity を組み立てる。
///
/// Mastodon の慣習に合わせて `to = [Public]`、`cc = [followers]`。`object` は
/// 移動元 (= 自分) の URI、`target` は移動先の URI。
///
/// **id 形式** ([[m9-pr1-review]] round-2 [2]): `#fragment` を使うと
/// JSON-LD として「同一ドキュメント内 anchor」扱いされ、Misskey 等が
/// キャッシュ / 重複排除で混乱する可能性がある。Mastodon と同じ
/// `<ap_id>/activities/move-<ts>` の path 形式に揃える。
fn build_move_activity(actor: &ActorRow, target: &str) -> JsonValue {
    let now = chrono::Utc::now();
    let activity_id = format!(
        "{ap_id}/activities/move-{ts}",
        ap_id = actor.ap_id,
        ts = now.timestamp_millis(),
    );
    let followers = actor
        .followers_url
        .clone()
        .unwrap_or_else(|| format!("{}/followers", actor.ap_id));
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Move",
        "id": activity_id,
        "actor": actor.ap_id,
        "object": actor.ap_id,
        "target": target,
        "to": [PUBLIC_URI],
        "cc": [followers],
        "published": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

async fn enqueue_to_followers(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
) -> usize {
    let inboxes = match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "list_accepted_inboxes failed");
            return 0;
        }
    };
    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_) => queued += 1,
            Err(err) => warn!(?err, %inbox, "enqueue failed"),
        }
    }
    // CLI (move-out) 経路ではワーカ未稼働なので permit が貯まるだけ (= 次回
    // serve 起動時の pick_due が拾う)。serve 経路なら即配送される。
    if queued > 0 {
        state.wake_delivery();
    }
    queued
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_alias_uri_accepts_http_and_https() {
        validate_alias_uri("https://old.example/users/alice").unwrap();
        validate_alias_uri("http://old.example/users/alice").unwrap();
    }

    #[test]
    fn validate_alias_uri_rejects_non_http_schemes() {
        for bad in [
            "",
            "javascript:alert(1)",
            "mailto:alice@example.com",
            "file:///etc/passwd",
            "data:text/plain,hi",
            "ftp://example.com/x",
            "not a url",
            "/relative/path",
        ] {
            assert!(
                validate_alias_uri(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_alias_uri_requires_host_in_error_messages() {
        // 空文字 / scheme のみは Url::parse 段で落ちる。実装としては
        // `parsed` で先に Url パースエラーになるが、エラー文言にはどこかで
        // ヒントが入っていれば OK。
        let err = validate_alias_uri("https://").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid alias URI") || msg.contains("host"),
            "unexpected error message: {msg}",
        );
    }

    #[test]
    fn move_activity_has_required_fields() {
        let actor = ActorRow {
            id: 1,
            ap_id: "https://x.test/users/alice".into(),
            preferred_username: "alice".into(),
            host: "x.test".into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: "https://x.test/users/alice/inbox".into(),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: Some("https://x.test/users/alice/followers".into()),
            following_url: None,
            public_key_id: "https://x.test/users/alice#main-key".into(),
            public_key_pem: "PEM".into(),
            private_key_pem: Some("PRIV".into()),
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: sqlx::types::Json(vec![]),
            moved_to_ap_id: Some("https://new.test/users/alice".into()),
            is_local: true,
            actor_type: "Person".into(),
            manually_approves_followers: false,
            fetched_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let a = build_move_activity(&actor, "https://new.test/users/alice");
        assert_eq!(a["type"], "Move");
        assert_eq!(a["actor"], "https://x.test/users/alice");
        assert_eq!(a["object"], "https://x.test/users/alice");
        assert_eq!(a["target"], "https://new.test/users/alice");
        let cc = a["cc"].as_array().unwrap();
        assert!(
            cc.iter()
                .any(|v| v == "https://x.test/users/alice/followers")
        );
        let id = a["id"].as_str().unwrap();
        // path 形式 `<ap_id>/activities/move-<ts>` (Mastodon 互換)。
        // `#` fragment は使わない ([[m9-pr1-review]] round-2 [2])。
        assert!(
            id.starts_with("https://x.test/users/alice/activities/move-"),
            "id should be a dereferenceable path, got {id}",
        );
        assert!(
            !id.contains('#'),
            "id must not contain # fragment, got {id}"
        );
    }
}
