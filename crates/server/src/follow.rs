//! `follow <acct>` CLI (M10) — 単発で `acct:user@host` 宛に Follow を送る管理操作。
//!
//! # 流れ
//!
//! 1. ローカル actor を解決する (`server.user`/`server.host`)。
//! 2. `media-proxy` の `POST /v1/webfinger/resolve` で `acct` を actor URI に解決する。
//!    `WebFinger` 取得は JSON 通信だが、外向き接続を媒介に寄せて server コンテナの
//!    egress を絞る (M10)。
//! 3. `remote_actor::fetch_and_upsert` で actor JSON を取得して DB に書き込む。
//! 4. 既存 Follow 行があれば状態をチェック (= `accepted` なら何もせず終了、
//!    `pending` は再 enqueue、`rejected` は明示拒否)。
//! 5. `follow_ap_id` を決定論的に組み立て (`follow-cli-{follower}-{followed}`)、
//!    `repo::follow::upsert_pending` で行を確保する。
//! 6. Follow activity を `delivery_queue` に投入する。常駐ワーカが拾って送る。
//!
//! # 何故 follow-cli- prefix を使うか
//!
//! 既存の `enqueue_auto_refollow` (M9 `dispatch/move_handler`) が
//! `follow-move-{old_follow_id}` を使っており、ここと衝突しない identifier に
//! することで「どこから生まれた Follow か」が `ap_id` だけで追える。CLI 起源の
//! Follow は (follower, followed) ペアで一意なので id に `{follower}-{followed}`
//! を含める ── 同じ相手に何度叩いても同じ activity id になり (= 相手側で
//! 自然と冪等になる)、`(follower_actor_id, followed_actor_id)` UNIQUE 制約と
//! も整合する。

use anyhow::{Context, bail};
use sakurasato_core::model::{ActorRow, FollowRow, FollowState};
use sakurasato_core::{Config, repo};
use serde_json::{Value as JsonValue, json};
use tracing::{info, warn};

use crate::cli::FollowArgs;
use crate::delivery;
use crate::media_proxy_client::MediaProxyError;
use crate::remote_actor::{self, FetchError};
use crate::state::AppState;

pub async fn run(config: Config, args: FollowArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let local = resolve_local_actor(&state).await?;
    let target = resolve_target(&state, &args).await?;

    if target.id == local.id {
        bail!(
            "refusing to follow our own local actor {:?}; nothing to do",
            local.ap_id,
        );
    }
    if let Some(moved) = target.moved_to_ap_id.as_deref() {
        // moved_to が立っている actor を follow するのは設計上問題ないが
        // (= 相手側で Move を発行している = 配送先が新 actor に流れる)、
        // CLI から明示的に follow する意図は薄いので警告だけ出す。
        warn!(
            target = %target.ap_id,
            moved_to = moved,
            "target actor has movedTo set; consider following the new actor instead",
        );
    }

    let follow_ap_id = format!(
        "https://{host}/users/{user}/activities/follow-cli-{follower}-{followed}",
        host = state.config().server.host,
        user = local.preferred_username,
        follower = local.id,
        followed = target.id,
    );

    let row = repo::follow::upsert_pending(state.pool(), &follow_ap_id, local.id, target.id)
        .await
        .context("upsert pending follow row")?;

    match parse_follow_state(&row)? {
        FollowState::Accepted => {
            println!(
                "already following {target}: follow_id={id} state=accepted (no Follow sent)",
                target = target.ap_id,
                id = row.id,
            );
            return Ok(());
        }
        FollowState::Rejected => {
            bail!(
                "previous Follow to {target} was rejected; aborting. \
                 If you want to retry, delete the follow row first (id={id}).",
                target = target.ap_id,
                id = row.id,
            );
        }
        FollowState::Pending => {
            info!(
                follow_id = row.id,
                target = %target.ap_id,
                "existing pending follow row reused; re-queueing Follow delivery",
            );
        }
    }

    let activity = build_follow_activity(&follow_ap_id, &local.ap_id, &target.ap_id);
    let inbox = target
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target.inbox_url);
    let queued = delivery::enqueue_activity(state.pool(), local.id, inbox, &activity)
        .await
        .with_context(|| format!("enqueue Follow to {inbox}"))?;

    println!(
        "queued Follow to {target}: follow_id={follow_id} delivery_queue_id={queue_id} inbox={inbox}",
        target = target.ap_id,
        follow_id = row.id,
        queue_id = queued.id,
        inbox = inbox,
    );
    Ok(())
}

/// `--actor-uri <url>` 直接指定があればそれを使い、無ければ `acct` を `WebFinger`
/// で解決する。どちらの経路でも最終的に `remote_actor::fetch_and_upsert` で
/// DB に actor 行を取り込む。
async fn resolve_target(state: &AppState, args: &FollowArgs) -> anyhow::Result<ActorRow> {
    let actor_uri = if let Some(uri) = args.actor_uri.as_deref() {
        info!(
            actor_uri = uri,
            "follow: using --actor-uri (skipping WebFinger)"
        );
        uri.to_string()
    } else {
        // `--actor-uri` 無しのときは `acct` が `required_unless_present` で
        // 必須になっているため、ここに来た時点で `acct` は Some。
        // 念のため `unwrap_or_default()` で空文字に倒し、media-proxy 側の
        // `parse_acct` で `invalid_acct` を返させる (= clap 側を素通りした
        // 異常系でも `unwrap()` panic を避ける)。
        let acct = args.acct.as_deref().unwrap_or_default();
        let resolved = state
            .media_proxy()
            .resolve_webfinger(acct)
            .await
            .map_err(map_media_proxy_err)?;
        info!(
            acct = %acct,
            subject = %resolved.subject,
            actor_uri = %resolved.actor_uri,
            "follow: WebFinger resolved",
        );
        // **PR #78 review F-1 (cross-domain hijack 防御)**: WebFinger が
        // 返した actor_uri のホストが、ユーザが指定した acct のホストと
        // 一致するか検証する。CLI 経路は管理者操作だが、誤って公開された
        // WebFinger サーバから差し替えを引かないよう同じ防御を入れる。
        if let Some(host) = extract_acct_host(acct) {
            ensure_webfinger_host_match(&host, &resolved.actor_uri)?;
        }
        resolved.actor_uri
    };

    remote_actor::fetch_and_upsert(state, &actor_uri)
        .await
        .map_err(|e| match e {
            FetchError::Blocked { host, reason } => {
                anyhow::anyhow!("remote fetch blocked: host {host:?} → {reason}")
            }
            FetchError::Malformed(msg) => anyhow::anyhow!("remote actor malformed: {msg}"),
            FetchError::Db(err) => anyhow::Error::new(err).context("upsert remote actor"),
            other => anyhow::anyhow!("remote actor fetch failed: {other}"),
        })
}

/// **PR #78 review F-1**: `acct` (= `user@host` / `@user@host` / `acct:user@host`)
/// から host 部だけを lower-case で抜き出す。形式が壊れていれば `None`。
/// `media-proxy::webfinger::parse_acct` と同じ受理形を踏襲する。
fn extract_acct_host(acct: &str) -> Option<String> {
    let trimmed = acct.trim();
    let body = trimmed
        .strip_prefix("acct:")
        .unwrap_or(trimmed)
        .trim_start_matches('@');
    let (_, host) = body.split_once('@')?;
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// **PR #78 review F-1**: `WebFinger` が返した `actor_uri` のホストが、
/// クエリした acct のホストと一致するかを検証する。不一致は cross-domain
/// 差し替え攻撃の徴候として拒否する。
fn ensure_webfinger_host_match(expected_host_lc: &str, actor_uri: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(actor_uri)
        .with_context(|| format!("WebFinger returned invalid actor_uri {actor_uri:?}"))?;
    let actor_host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("WebFinger actor_uri {actor_uri:?} has no host"))?
        .to_ascii_lowercase();
    if actor_host == expected_host_lc {
        return Ok(());
    }
    bail!(
        "WebFinger returned actor_uri on different host (expected {expected_host_lc:?}, \
         got {actor_host:?}); possible cross-domain redirect, refusing to follow",
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

async fn resolve_local_actor(state: &AppState) -> anyhow::Result<ActorRow> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .context("query local actor")?;
    match row {
        Some(a) if a.is_local => Ok(a),
        Some(_) => bail!(
            "actor for {user}@{host} exists but is not local; database is in an inconsistent state"
        ),
        None => bail!("no local actor for {user}@{host}; run `sakurasato-server init` first"),
    }
}

fn parse_follow_state(row: &FollowRow) -> anyhow::Result<FollowState> {
    match row.state.as_str() {
        "pending" => Ok(FollowState::Pending),
        "accepted" => Ok(FollowState::Accepted),
        "rejected" => Ok(FollowState::Rejected),
        other => bail!("follow row {} has unknown state {other:?}", row.id),
    }
}

fn map_media_proxy_err(err: MediaProxyError) -> anyhow::Error {
    match err {
        MediaProxyError::Upstream {
            status,
            reason,
            message,
        } => anyhow::anyhow!(
            "media-proxy WebFinger resolve failed (HTTP {status}, reason={reason}): {message}",
        ),
        other => anyhow::anyhow!("media-proxy WebFinger resolve failed: {other}"),
    }
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

    /// **PR #78 review F-1**: `extract_acct_host` は 3 形式 (acct: / @ / 素)
    /// すべてから host を lower-case で取り出す。
    #[test]
    fn extract_acct_host_handles_all_three_forms() {
        assert_eq!(
            extract_acct_host("acct:alice@Example.com").as_deref(),
            Some("example.com"),
        );
        assert_eq!(
            extract_acct_host("@alice@example.com").as_deref(),
            Some("example.com"),
        );
        assert_eq!(
            extract_acct_host("alice@example.com").as_deref(),
            Some("example.com"),
        );
        // 形式不正は None。
        assert_eq!(extract_acct_host("no-at-sign"), None);
        assert_eq!(extract_acct_host("alice@"), None);
    }

    #[test]
    fn ensure_webfinger_host_match_rejects_cross_domain() {
        ensure_webfinger_host_match("evil.example", "https://evil.example/users/bob").unwrap();
        let err = ensure_webfinger_host_match("evil.example", "https://victim.example/users/bob")
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("different host"), "msg={msg}");
        assert!(msg.contains("victim.example"), "msg={msg}");
    }

    #[test]
    fn ensure_webfinger_host_match_is_case_insensitive() {
        ensure_webfinger_host_match("evil.example", "https://EVIL.EXAMPLE/users/bob").unwrap();
    }
}
