//! `sakurasato-server notification-channel <add|list|remove|enable|disable|test>` の実体。
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
    NotificationChannelEnableArgs, NotificationChannelEventArgs, NotificationChannelIdArgs,
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
        NotificationChannelCommand::Enable(args) => run_enable(&state, args).await,
        NotificationChannelCommand::Disable(args) => run_disable(&state, args).await,
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
            "id={} name={:?} format={} host={} events={}",
            row.id, row.name, row.format, host, events,
        );
    }
    Ok(())
}

/// `list` 出力で「ON になっている event」を CLI 表示ラベル (kebab) で返す。
/// 列名と event の対応は [`NotificationEvent::display_label`] に集約する
/// (= `enable` / `disable` の成功メッセージと同じ表記揺れを起こさない)。
fn enabled_events(row: &NotificationChannelRow) -> Vec<&'static str> {
    let mut out = Vec::new();
    if row.notify_mention {
        out.push(NotificationEvent::Mention.display_label());
    }
    if row.notify_direct {
        out.push(NotificationEvent::Direct.display_label());
    }
    if row.notify_quote {
        out.push(NotificationEvent::Quote.display_label());
    }
    if row.notify_reaction {
        out.push(NotificationEvent::Reaction.display_label());
    }
    if row.notify_renote {
        out.push(NotificationEvent::Renote.display_label());
    }
    if row.notify_follow {
        out.push(NotificationEvent::Follow.display_label());
    }
    if row.notify_follow_request {
        out.push(NotificationEvent::FollowRequest.display_label());
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

/// `enable` ハンドラ。
///
/// - `--only` 無し (= 既定): 指定 event だけ TRUE、他列は据え置き (部分更新)。
/// - `--only`: 指定 event を TRUE、他列を FALSE に倒す (完全宣言)。
///   `--only all` は意味が無いので拒否する (= `all` は単独で「全 ON」を
///   表すため、`--only` を付けると「他は OFF」 と矛盾する解釈になる)。
async fn run_enable(state: &AppState, args: NotificationChannelEnableArgs) -> anyhow::Result<()> {
    // PR #147 round-2 F-1: `--only` ガードは **`all` token を入力したか** で
    // 判定する。`events.len() == 7` で判定すると、全 7 event を明示列挙した
    // 正当なケース (= `--only mention,direct,quote,reaction,renote,follow,follow-request`)
    // も誤って弾いてしまう (parse_events は両者とも 7 要素 Vec を返すため
    // 長さでは区別不能)。
    let used_all_token = args.events.iter().any(|t| t.eq_ignore_ascii_case("all"));
    if args.only && used_all_token {
        bail!(
            "`--only all` is meaningless (all events already covered); \
             use `enable --id N all` without `--only` for a full-on state"
        );
    }
    let events = parse_events(&args.events)?;
    let updated = if args.only {
        repo::notification_channel::set_exact_state(state.pool(), args.id, &events)
            .await
            .with_context(|| format!("set exact state for id={}", args.id))?
    } else {
        repo::notification_channel::set_events(state.pool(), args.id, &events, true)
            .await
            .with_context(|| format!("enable events for id={}", args.id))?
    };
    if !updated {
        bail!("no notification channel with id={}", args.id);
    }
    // PR #147 round-2 F-3: CLI display は kebab に統一 (= `list` 出力と一致)。
    // `as_str()` は wire 表現 (`delivery_queue.activity.event` JSONB / log) で
    // snake のまま保つ ── 永続化されたペイロードと互換を取るため。
    let event_list = events
        .iter()
        .map(|e| e.display_label())
        .collect::<Vec<_>>()
        .join(",");
    let mode = if args.only { " (--only)" } else { "" };
    let id = args.id;
    println!("enabled channel id={id} events={event_list}{mode}");
    Ok(())
}

/// `disable` ハンドラ。常に部分更新 (= 指定 event のみ FALSE、他列据え置き)。
/// 「全部 OFF にして特定だけ ON」は `enable --only` 経由で表現する。
async fn run_disable(state: &AppState, args: NotificationChannelEventArgs) -> anyhow::Result<()> {
    let events = parse_events(&args.events)?;
    let updated = repo::notification_channel::set_events(state.pool(), args.id, &events, false)
        .await
        .with_context(|| format!("disable events for id={}", args.id))?;
    if !updated {
        bail!("no notification channel with id={}", args.id);
    }
    let event_list = events
        .iter()
        .map(|e| e.display_label())
        .collect::<Vec<_>>()
        .join(",");
    let id = args.id;
    println!("disabled channel id={id} events={event_list}");
    Ok(())
}

/// CLI から渡された event 名トークン群を [`NotificationEvent`] 列に変換する。
///
/// - `all` は単独でのみ許可 (他 token と混ぜると拒否)。展開すると 7 個。
/// - それ以外は 1 個ずつ [`NotificationEvent::from_str`] で検証 → 不正値は
///   早期 bail (= DB を一切触らない)。
/// - 重複は保持して返す (= dedup は repo 側で行う ── 「同じ event を 2 回
///   並べたら CLI エラー」だと混乱するので寛容に倒す)。
fn parse_events(tokens: &[String]) -> anyhow::Result<Vec<NotificationEvent>> {
    if tokens.is_empty() {
        // clap の required = true で防いでいるが念のため。
        bail!("expected at least one event name");
    }
    let has_all = tokens.iter().any(|t| t.eq_ignore_ascii_case("all"));
    if has_all {
        if tokens.len() > 1 {
            bail!(
                "event 'all' cannot be combined with other events; \
                 use either 'all' alone or list events explicitly (got {tokens:?})"
            );
        }
        return Ok(NotificationEvent::all().to_vec());
    }
    tokens
        .iter()
        .map(|t| NotificationEvent::from_str(t).map_err(|e| anyhow!(e)))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 単数 token は 1 要素 Vec として通る。`mention` → `[Mention]`。
    #[test]
    fn parse_events_single_token() {
        let got = parse_events(&["mention".to_string()]).unwrap();
        assert_eq!(got, vec![NotificationEvent::Mention]);
    }

    /// kebab-case `follow-request` を `FollowRequest` として認識する。
    #[test]
    fn parse_events_kebab_case() {
        let got = parse_events(&["follow-request".to_string()]).unwrap();
        assert_eq!(got, vec![NotificationEvent::FollowRequest]);
    }

    /// clap が `--id 1 mention,quote` を `Vec!["mention", "quote"]` に
    /// splits した想定。両方 `NotificationEvent` に解決する。
    #[test]
    fn parse_events_multiple_tokens() {
        let got = parse_events(&["mention".to_string(), "quote".to_string()]).unwrap();
        assert_eq!(
            got,
            vec![NotificationEvent::Mention, NotificationEvent::Quote],
        );
    }

    /// `all` 単独で 7 個全部に展開される (順序は `NotificationEvent::all()` と
    /// 一致)。
    #[test]
    fn parse_events_all_expands_to_seven() {
        let got = parse_events(&["all".to_string()]).unwrap();
        assert_eq!(got, NotificationEvent::all().to_vec());
        assert_eq!(got.len(), 7);
    }

    /// 大文字小文字を問わず `all` を `all` と認識する。
    #[test]
    fn parse_events_all_case_insensitive() {
        let got = parse_events(&["ALL".to_string()]).unwrap();
        assert_eq!(got, NotificationEvent::all().to_vec());
    }

    /// `all` + 他 event の混在は **拒否** する (「全部 + α」は意味が無く誤入力
    /// の可能性が高いので寛容に倒さない設計)。
    #[test]
    fn parse_events_all_with_other_rejected() {
        let err = parse_events(&["all".to_string(), "mention".to_string()]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'all'"), "error should mention 'all': {msg}");
        assert!(
            msg.contains("cannot be combined"),
            "error should explain rejection: {msg}",
        );
    }

    /// 不正な event 名は早期 bail (= DB を一切触らない)。
    #[test]
    fn parse_events_unknown_token_rejected() {
        let err = parse_events(&["mentions".to_string()]).unwrap_err(); // typo
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown notification event"),
            "error should mention unknown event: {msg}",
        );
    }

    /// 空 Vec は clap で防がれているが、防御的に bail。
    #[test]
    fn parse_events_empty_rejected() {
        let err = parse_events(&[]).unwrap_err();
        assert!(format!("{err}").contains("expected at least one event"));
    }

    /// 重複 token は **拒否しない** (= 寛容に dedup は repo 側に任せる)。
    /// CLI レベルで重複エラーを出すと shell の `event1 event1` のような
    /// うっかりや、`enable mention,mention` のスクリプト生成ミスを罰しすぎる。
    #[test]
    fn parse_events_duplicate_tokens_passed_through() {
        let got = parse_events(&["mention".to_string(), "mention".to_string()]).unwrap();
        assert_eq!(
            got,
            vec![NotificationEvent::Mention, NotificationEvent::Mention],
        );
        // repo::set_events の dedup ロジックで最終 SQL は単一列 SET に倒れる。
    }

    /// **PR #147 round-2 F-1 回帰テスト**: 全 7 event を明示列挙する
    /// `--only mention,direct,quote,reaction,renote,follow,follow-request`
    /// は `parse_events` で 7 要素 Vec に展開されるが、これは `--only all`
    /// とは別経路 (= ガードに引っかかってはならない)。`run_enable` の判定
    /// ロジックを直接呼べないので、ガード式を同じ条件で組み立てて検証する。
    #[test]
    fn run_enable_only_with_seven_explicit_events_is_allowed() {
        let tokens = vec![
            "mention".to_string(),
            "direct".to_string(),
            "quote".to_string(),
            "reaction".to_string(),
            "renote".to_string(),
            "follow".to_string(),
            "follow-request".to_string(),
        ];
        // F-1 修正のガード式と一致させる: `all` トークンが含まれているかで判定。
        let used_all_token = tokens.iter().any(|t| t.eq_ignore_ascii_case("all"));
        assert!(
            !used_all_token,
            "7 個明示列挙は `all` トークン未使用なので `--only` ガードに引っかからない",
        );
        let parsed = parse_events(&tokens).unwrap();
        assert_eq!(
            parsed.len(),
            7,
            "全 7 event が NotificationEvent に解決される"
        );
    }

    /// 対比: `--only all` の側はちゃんとガードに引っかかる。
    #[test]
    fn run_enable_only_all_token_is_rejected_by_guard() {
        let tokens = ["all".to_string()];
        let used_all_token = tokens.iter().any(|t| t.eq_ignore_ascii_case("all"));
        assert!(used_all_token, "`all` トークンはガードを発火させる");
    }

    /// F-3: `display_label()` は CLI 入力 (kebab) と完全一致するので、出力を
    /// scripting で再入力したときに往復する。
    #[test]
    fn display_label_is_kebab_case() {
        assert_eq!(
            NotificationEvent::FollowRequest.display_label(),
            "follow-request"
        );
        // wire 表現 (as_str) は snake_case のまま (= delivery_queue.activity.event 互換)。
        assert_eq!(NotificationEvent::FollowRequest.as_str(), "follow_request");
    }
}
