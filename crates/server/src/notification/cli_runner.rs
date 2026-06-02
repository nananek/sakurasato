//! `sakurasato-server notification-channel <add|list|remove|toggle|test>` の実体。
//!
//! ハンドラは状態 (`AppState`) を受け、`notification_channel` repo と
//! `delivery::enqueue_activity` だけを操作する。実 HTTP POST は worker 側
//! ([`crate::delivery::attempt_post_webhook`]) に任せる。
//!
//! # 表示方針 (`list`)
//!
//! Webhook URL は **host だけ** 出して path / query は伏せる。URL 全体は
//! それ自体が capability なので、誤って screenshot や paste で漏らされる
//! のを避ける。完全な URL は psql で `SELECT url FROM notification_channel`
//! すれば取れるので運用上の困りごとは無い。

use std::str::FromStr;

use anyhow::{Context, anyhow, bail};
use chrono::Utc;
use sakurasato_core::Config;
use sakurasato_core::model::{NotificationChannelRow, NotificationEvent};
use sakurasato_core::repo;
use sakurasato_core::repo::notification_channel::NewNotificationChannel;
use serde_json::json;
use tracing::info;

use crate::cli::{
    NotificationChannelAddArgs, NotificationChannelArgs, NotificationChannelCommand,
    NotificationChannelIdArgs, NotificationChannelToggleArgs,
};
use crate::delivery;
use crate::net_guard;
use crate::state::AppState;

use super::payload::{NotificationContext, WebhookFormat, build_payload};

pub async fn run(config: Config, args: NotificationChannelArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        NotificationChannelCommand::Add(add) => run_add(&state, add).await,
        NotificationChannelCommand::List => run_list(&state).await,
        NotificationChannelCommand::Remove(rm) => run_remove(&state, rm).await,
        NotificationChannelCommand::Toggle(toggle) => run_toggle(&state, toggle).await,
        NotificationChannelCommand::Test(test) => run_test(&state, test).await,
    }
}

async fn run_add(state: &AppState, add: NotificationChannelAddArgs) -> anyhow::Result<()> {
    let name = add.name.trim().to_string();
    if name.is_empty() {
        bail!("--name must not be empty");
    }
    let format = match add.format.as_str() {
        "embed" | "plain" => add.format.clone(),
        other => bail!("--format must be 'embed' or 'plain', got {other:?}"),
    };

    // 登録時の SSRF 検査: private / loopback / link-local / reserved を弾く。
    // 配送時にも再検査される (TOCTOU 防御)。`reqwest::Url::parse` で http /
    // https 以外のスキームと host 欠落も同時に弾く。
    let parsed = reqwest::Url::parse(&add.url)
        .with_context(|| format!("invalid webhook URL {:?}", add.url))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "webhook URL scheme must be http or https, got {:?}",
            parsed.scheme()
        );
    }
    if parsed.host_str().is_none() {
        bail!("webhook URL must have a host component");
    }
    if let Some(reason) = net_guard::host_blocked(&parsed) {
        bail!(
            "webhook URL host {:?} is blocked ({reason})",
            parsed.host_str().unwrap_or("")
        );
    }

    let row = repo::notification_channel::insert(
        state.pool(),
        NewNotificationChannel {
            name,
            url: add.url,
            format,
        },
    )
    .await
    .context("insert notification_channel row")?;

    info!(
        id = row.id,
        name = %row.name,
        host = parsed.host_str().unwrap_or(""),
        format = %row.format,
        "notification channel added",
    );
    println!(
        "added channel id={} name={:?} host={} format={}",
        row.id,
        row.name,
        parsed.host_str().unwrap_or(""),
        row.format,
    );
    Ok(())
}

async fn run_list(state: &AppState) -> anyhow::Result<()> {
    let rows = repo::notification_channel::list_all(state.pool())
        .await
        .context("list notification_channel rows")?;
    if rows.is_empty() {
        println!("(no notification channels)");
        return Ok(());
    }
    for row in rows {
        // URL 全体は伏せて host だけ出す (capability 漏洩を避ける)。
        let host = reqwest::Url::parse(&row.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "?".into());
        let events = enabled_events(&row).join(",");
        let events = if events.is_empty() {
            "(none)".to_string()
        } else {
            events
        };
        println!(
            "id={} name={:?} enabled={} format={} host={} events={}",
            row.id, row.name, row.enabled, row.format, host, events,
        );
    }
    Ok(())
}

fn enabled_events(row: &NotificationChannelRow) -> Vec<&'static str> {
    let mut out = Vec::new();
    if row.notify_mention {
        out.push("mention");
    }
    if row.notify_direct {
        out.push("direct");
    }
    if row.notify_quote {
        out.push("quote");
    }
    if row.notify_reaction {
        out.push("reaction");
    }
    if row.notify_renote {
        out.push("renote");
    }
    if row.notify_follow {
        out.push("follow");
    }
    if row.notify_follow_request {
        out.push("follow-request");
    }
    out
}

async fn run_remove(state: &AppState, args: NotificationChannelIdArgs) -> anyhow::Result<()> {
    let deleted = repo::notification_channel::delete_by_id(state.pool(), args.id)
        .await
        .with_context(|| format!("delete notification_channel id={}", args.id))?;
    if !deleted {
        bail!("no notification channel with id={}", args.id);
    }
    println!("removed channel id={}", args.id);
    Ok(())
}

async fn run_toggle(state: &AppState, args: NotificationChannelToggleArgs) -> anyhow::Result<()> {
    let updated = if args.event.eq_ignore_ascii_case("all") {
        repo::notification_channel::toggle_enabled(state.pool(), args.id)
            .await
            .with_context(|| format!("toggle enabled for id={}", args.id))?
    } else {
        let event = NotificationEvent::from_str(&args.event).map_err(|e| anyhow!(e))?;
        repo::notification_channel::toggle_event(state.pool(), args.id, event)
            .await
            .with_context(|| {
                format!(
                    "toggle event {event} for id={id}",
                    event = event.as_str(),
                    id = args.id,
                )
            })?
    };
    if !updated {
        bail!("no notification channel with id={}", args.id);
    }
    println!("toggled channel id={} event={}", args.id, args.event);
    Ok(())
}

async fn run_test(state: &AppState, args: NotificationChannelIdArgs) -> anyhow::Result<()> {
    let channel = repo::notification_channel::get_by_id(state.pool(), args.id)
        .await
        .with_context(|| format!("fetch notification_channel id={}", args.id))?
        .ok_or_else(|| anyhow!("no notification channel with id={}", args.id))?;

    // local actor は test ペイロードの `author` に出す。未 init の状態で test
    // を撃つのは無理筋なので、見つからなければ早期失敗。
    let host = state.config().server.host.clone();
    let user = state.config().server.user.clone();
    let actor = repo::actor::get_by_username_host(state.pool(), &user, &host)
        .await
        .context("lookup local actor")?
        .ok_or_else(|| {
            anyhow!("local actor {user}@{host} not initialised; run `sakurasato-server init`")
        })?;

    let ctx = NotificationContext {
        actor: &actor,
        instance_host: &host,
        note: None,
        reaction_content: None,
        quote_target: None,
        occurred_at: Utc::now(),
    };
    let format = WebhookFormat::from_db(&channel.format);
    let payload = build_payload(NotificationEvent::Follow, format, &ctx);
    // 固定文言を上書きしてテスト通知だと一目で分かるようにする。
    let payload = override_test_text(payload, format);
    let wire_type = match format {
        WebhookFormat::Embed => "Webhook:Discord",
        WebhookFormat::Plain => "Webhook:Plain",
    };
    let activity = json!({
        "type": wire_type,
        "channel_id": channel.id,
        "event": "test",
        "payload": payload,
    });
    let queued = delivery::enqueue_activity(state.pool(), actor.id, &channel.url, &activity)
        .await
        .context("enqueue test webhook activity")?;
    println!(
        "enqueued test notification: channel_id={} queue_id={}",
        channel.id, queued.id,
    );
    Ok(())
}

/// テスト通知だけは「テスト通知です」と上書きする (Follow 流用だと
/// 「自分がフォローされた」誤読を招くため)。embed なら title、plain なら content。
fn override_test_text(mut payload: serde_json::Value, format: WebhookFormat) -> serde_json::Value {
    match format {
        WebhookFormat::Embed => {
            if let Some(arr) = payload
                .get_mut("embeds")
                .and_then(serde_json::Value::as_array_mut)
                && let Some(first) = arr.get_mut(0).and_then(serde_json::Value::as_object_mut)
            {
                first.insert(
                    "title".into(),
                    serde_json::Value::String("テスト通知".into()),
                );
                first.insert(
                    "description".into(),
                    serde_json::Value::String(
                        "Sakurasato からの疎通テストです。これが届いていれば設定 OK。".into(),
                    ),
                );
            }
        }
        WebhookFormat::Plain => {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "content".into(),
                    serde_json::Value::String(
                        "[テスト通知] Sakurasato からの疎通テストです。".into(),
                    ),
                );
            }
        }
    }
    payload
}
