//! 通知を `delivery_queue` に enqueue する fan-out 層。
//!
//! AP dispatch (`crate::dispatch::{note,reaction,announce,handler}`) が DB 書き
//! 込み commit 後に **fire-and-forget** で呼ぶ。本層は次の責務だけを持つ:
//!
//! 1. [`repo::notification_channel::list_enabled_for_event`] で対象 channel を
//!    取得する
//! 2. 各 channel ごとに [`super::payload::build_payload`] で integer JSON を組む
//! 3. `delivery_queue.activity` に `{"type": "Webhook:<Discord|Plain>", channel_id,
//!    event, payload}` を入れて 1 行 enqueue する
//!
//! # 失敗の扱い
//!
//! 本層から `Err` を上に返すと AP dispatch が失敗扱いになり、相手側が retry
//! してくる ── webhook 通知の失敗が連合配送を巻き込むのは本末転倒なので、
//! **すべての失敗を内部で `warn!` で握り潰す**。`sender_actor_id` lookup や
//! `enqueue_activity` の失敗も同様。
//!
//! # `sender_actor_id` の扱い
//!
//! `delivery_queue.sender_actor_id` は FK で `actor.id` を要求する。webhook
//! 配送は署名しないので鍵を必要としないが、列自体は NOT NULL なので local
//! actor の id を埋める。worker 側は `activity.type` が `Webhook:` prefix の
//! ときは sender の鍵を一切触らない ([`crate::delivery::attempt_post`])。

use sakurasato_core::model::{ActorRow, NoteRow, NotificationChannelRow, NotificationEvent};
use sakurasato_core::repo;
use serde_json::json;
use tracing::warn;

use super::payload::{NotificationContext, WebhookFormat, build_payload};
use crate::delivery;
use crate::state::AppState;

/// 対象 event を有効化している全 channel に通知を 1 行ずつ enqueue する。
///
/// 失敗は内部で握り潰す (`warn!` のみ)。AP dispatch を巻き込まない。
pub(crate) async fn notify(
    state: &AppState,
    event: NotificationEvent,
    ctx: &NotificationContext<'_>,
) {
    // recipient (= local user) を解決。お一人様サーバなので常に同一。失敗時は
    // 全通知を諦める (本層は best-effort で AP dispatch を巻き込まない)。
    let Some(local_actor_id) = resolve_local_actor_id(state).await else {
        return;
    };

    // **in-app 通知フィード** (`notification` テーブル) に 1 行貯める ── webhook
    // channel の有無に関わらず常に。TUI / MiAuth (Aria) がここから一覧する。
    record_in_app(state, event, ctx, local_actor_id).await;

    // **webhook 通知** (既存) ── enabled な channel にのみ enqueue。
    let channels =
        match repo::notification_channel::list_enabled_for_event(state.pool(), event).await {
            Ok(rows) => rows,
            Err(err) => {
                warn!(
                    ?err,
                    event = event.as_str(),
                    "notification: list channels failed"
                );
                return;
            }
        };

    if channels.is_empty() {
        return;
    }

    for channel in channels {
        if let Err(err) = enqueue_for_channel(state, &channel, event, ctx, local_actor_id).await {
            warn!(
                ?err,
                event = event.as_str(),
                channel_id = channel.id,
                "notification: enqueue failed",
            );
        }
    }
}

/// in-app 通知フィード (`notification` テーブル) に 1 行 insert する。best-effort
/// (失敗は `warn!` のみで AP dispatch / webhook を巻き込まない)。`NotificationContext`
/// の notifier actor / 対象 note / reaction をそのまま列に落とす。
async fn record_in_app(
    state: &AppState,
    event: NotificationEvent,
    ctx: &NotificationContext<'_>,
    recipient_actor_id: i64,
) {
    let new = repo::notification::NewNotification {
        recipient_actor_id,
        event_type: event.as_str().to_string(),
        notifier_actor_id: Some(ctx.actor.id),
        note_id: ctx.note.map(|n| n.id),
        reaction: ctx.reaction_content.map(str::to_string),
        created_at: ctx.occurred_at,
    };
    if let Err(err) = repo::notification::insert(state.pool(), new).await {
        warn!(
            ?err,
            event = event.as_str(),
            "notification: in-app insert failed"
        );
    }
}

async fn enqueue_for_channel(
    state: &AppState,
    channel: &NotificationChannelRow,
    event: NotificationEvent,
    ctx: &NotificationContext<'_>,
    local_actor_id: i64,
) -> anyhow::Result<()> {
    let format = WebhookFormat::from_db(&channel.format);
    let payload = build_payload(event, format, ctx);
    let wire_type = match format {
        WebhookFormat::Embed => "Webhook:Discord",
        WebhookFormat::Plain => "Webhook:Plain",
    };
    // `channel_id` / `event` は `payload` の外に出して運用トレース用に残す
    // (= 「どの channel に何の event を送ったか」を psql で grep できる)。
    // worker は `payload` だけを実 POST に使う。
    let activity = json!({
        "type": wire_type,
        "channel_id": channel.id,
        "event": event.as_str(),
        "payload": payload,
    });
    delivery::enqueue_activity(state.pool(), local_actor_id, &channel.url, &activity).await?;
    Ok(())
}

async fn resolve_local_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(actor)) if actor.is_local => Some(actor.id),
        Ok(Some(_)) => {
            warn!(%user, %host, "notification: configured actor is not local; skipping");
            None
        }
        Ok(None) => {
            warn!(%user, %host, "notification: local actor not initialised; skipping");
            None
        }
        Err(err) => {
            warn!(?err, "notification: local actor lookup failed");
            None
        }
    }
}

/// inbound `Create.Note` 用のヘルパ。`addresses_us` / quote target の有無を見て
/// `Mention` / `Direct` / `Quote` を **独立に** 発火する (重複は許容: mention
/// で webhook が飛び、quote でも別 webhook が飛ぶ)。
pub(crate) async fn notify_inbound_note(
    state: &AppState,
    signer: &ActorRow,
    note: &NoteRow,
    addresses_us: bool,
    quote_target: Option<&NoteRow>,
) {
    let host = state.config().server.host.clone();
    let occurred_at = chrono::Utc::now();
    let ctx = NotificationContext {
        actor: signer,
        instance_host: &host,
        note: Some(note),
        reaction_content: None,
        quote_target,
        occurred_at,
    };

    // Mention: 我々宛 (`to`/`cc` に local_ap_id を含む) かつ visibility が
    // public / unlisted / followers。Direct は別 event なので除外。
    //
    // Direct: 我々宛 かつ visibility が direct。`addresses_us` ガードを掛ける
    // のは「followee の第三者宛 DM が我々の inbox に届いた」ケースで誤通知し
    // ないため (`handle_create` は followee 投稿を `addresses_us = false` でも
    // 取り込むので、ここでフィルタしないと第三者 DM で Direct webhook が飛ぶ)。
    // Mention と完全対称化する。
    // (round-3 review F1)
    let visibility = note.visibility.as_str();
    if addresses_us && visibility != "direct" {
        notify(state, NotificationEvent::Mention, &ctx).await;
    }
    if addresses_us && visibility == "direct" {
        notify(state, NotificationEvent::Direct, &ctx).await;
    }
    if quote_target.is_some() {
        notify(state, NotificationEvent::Quote, &ctx).await;
    }
}

/// inbound `Like` / `EmojiReact` 用ヘルパ。
pub(crate) async fn notify_reaction(
    state: &AppState,
    signer: &ActorRow,
    target_note: &NoteRow,
    reaction_content: &str,
) {
    let host = state.config().server.host.clone();
    let ctx = NotificationContext {
        actor: signer,
        instance_host: &host,
        note: Some(target_note),
        reaction_content: Some(reaction_content),
        quote_target: None,
        occurred_at: chrono::Utc::now(),
    };
    notify(state, NotificationEvent::Reaction, &ctx).await;
}

/// inbound `Announce` 用ヘルパ。
pub(crate) async fn notify_renote(state: &AppState, signer: &ActorRow, target_note: &NoteRow) {
    let host = state.config().server.host.clone();
    let ctx = NotificationContext {
        actor: signer,
        instance_host: &host,
        note: Some(target_note),
        reaction_content: None,
        quote_target: None,
        occurred_at: chrono::Utc::now(),
    };
    notify(state, NotificationEvent::Renote, &ctx).await;
}

/// inbound `Follow` (auto-accept 経路) 用ヘルパ。
pub(crate) async fn notify_follow(state: &AppState, follower: &ActorRow) {
    let host = state.config().server.host.clone();
    let ctx = NotificationContext {
        actor: follower,
        instance_host: &host,
        note: None,
        reaction_content: None,
        quote_target: None,
        occurred_at: chrono::Utc::now(),
    };
    notify(state, NotificationEvent::Follow, &ctx).await;
}

/// inbound `Follow` (鍵アカ pending 経路) 用ヘルパ。
pub(crate) async fn notify_follow_request(state: &AppState, follower: &ActorRow) {
    let host = state.config().server.host.clone();
    let ctx = NotificationContext {
        actor: follower,
        instance_host: &host,
        note: None,
        reaction_content: None,
        quote_target: None,
        occurred_at: chrono::Utc::now(),
    };
    notify(state, NotificationEvent::FollowRequest, &ctx).await;
}
