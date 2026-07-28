//! Discord (embed) と Slack/Misskey 互換 plain の 2 形式の payload 生成。
//!
//! ここは純粋関数のみ。DB / HTTP / time/random source に触らないので unit test
//! が容易。`Utc::now()` は [`NotificationContext::occurred_at`] で受け取って
//! テスト可能にしている。
//!
//! # 配色 (Discord embed `color` 整数 = 24-bit RGB)
//!
//! | event           | 色   | hex      | rationale                       |
//! |-----------------|------|----------|---------------------------------|
//! | mention         | 青   | `4FC3F7` | 既存 Misskey の mention アイコン |
//! | direct          | 赤   | `E57373` | DM = 注意喚起                    |
//! | quote           | 橙   | `FFB74D` | 引用 = 中位優先度                |
//! | reaction        | 黄   | `FFD54F` | 軽量 interaction                 |
//! | renote          | 緑   | `81C784` | 拡散                             |
//! | follow          | 紫   | `BA68C8` | 関係性変更                       |
//! | follow_request  | 灰   | `9E9E9E` | 保留中                           |

use chrono::{DateTime, Utc};
use sakurasato_core::model::{ActorRow, NoteRow, NotificationEvent};
use serde_json::{Value as JsonValue, json};

/// Discord は 1 message 2000 文字制限、embed description は 4096。Discord 以外
/// (Slack 等) では更に短いこともあるが、UI で読みやすい量 = 500 字程度 + 省略
/// 記号で十分。footer に instance host を載せるので冗長な情報も削れる。
const TEXT_PREVIEW_CHARS: usize = 500;

/// Webhook の wire 形式。`notification_channel.format` 列の `'embed'` /
/// `'plain'` に対応。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebhookFormat {
    Embed,
    Plain,
}

impl WebhookFormat {
    /// DB 列 (`notification_channel.format` の `CHECK` 制約付き) と相互変換。
    pub(crate) fn from_db(s: &str) -> Self {
        match s {
            "plain" => Self::Plain,
            // `embed` 既定。CHECK 制約があるので `'embed'` 以外は来ない想定だが、
            // 万一既知でない値が来ても embed にフォールバックして黙る (= log 出さ
            // ない) ── webhook 通知が runtime panic で本筋 dispatch を巻き込まない
            // 設計。
            _ => Self::Embed,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Embed => "embed",
            Self::Plain => "plain",
        }
    }
}

/// イベントごとの埋め込み色 (24-bit RGB → Discord embed `color`)。
const fn color_for(event: NotificationEvent) -> u32 {
    match event {
        NotificationEvent::Mention => 0x4F_C3_F7,
        NotificationEvent::Direct => 0xE5_73_73,
        NotificationEvent::Quote => 0xFF_B7_4D,
        NotificationEvent::Reaction => 0xFF_D5_4F,
        NotificationEvent::Renote => 0x81_C7_84,
        NotificationEvent::Follow => 0xBA_68_C8,
        NotificationEvent::FollowRequest => 0x9E_9E_9E,
    }
}

/// イベントごとの日本語タイトル (embed) / [tag] (plain)。
const fn title_for(event: NotificationEvent) -> &'static str {
    match event {
        NotificationEvent::Mention => "メンション",
        NotificationEvent::Direct => "ダイレクト",
        NotificationEvent::Quote => "引用",
        NotificationEvent::Reaction => "リアクション",
        NotificationEvent::Renote => "リノート",
        NotificationEvent::Follow => "フォロー",
        NotificationEvent::FollowRequest => "フォローリクエスト",
    }
}

/// `dispatch` 側が組み立てて渡す context。actor は 1 人 (= 通知を引き起こした
/// 相手)。note は mention/direct/quote/reaction/renote で「対象 note」。quote
/// は `target_note` も別に持つ (= 「我々の note を引用した相手 note」の対応)。
#[derive(Debug)]
pub(crate) struct NotificationContext<'a> {
    /// 通知を引き起こした actor (mention 送信者 / reactor / boost した人 /
    /// follower)。
    pub actor: &'a ActorRow,
    /// 通知元 instance の host (footer.text に出す)。
    pub instance_host: &'a str,
    /// mention/direct/quote/reaction/renote の対象 Note。follow 系は `None`。
    pub note: Option<&'a NoteRow>,
    /// リアクションの内容 (Unicode emoji or `:shortcode:` / `:shortcode@host:`)。
    /// `Reaction` event のみ意味を持つ。
    pub reaction_content: Option<&'a str>,
    /// quote 時に「引用された自分の note」。`Quote` event のみ意味を持つ。
    pub quote_target: Option<&'a NoteRow>,
    /// 発生時刻 (embed.timestamp / plain content には出さない)。テストで固定
    /// 値を渡せるように DI している。
    pub occurred_at: DateTime<Utc>,
}

/// 入口。`format` で分岐して embed JSON または plain `{"content": "..."}` を返す。
pub(crate) fn build_payload(
    event: NotificationEvent,
    format: WebhookFormat,
    ctx: &NotificationContext<'_>,
) -> JsonValue {
    match format {
        WebhookFormat::Embed => build_embed(event, ctx),
        WebhookFormat::Plain => build_plain(event, ctx),
    }
}

fn build_embed(event: NotificationEvent, ctx: &NotificationContext<'_>) -> JsonValue {
    let mut embed = serde_json::Map::new();
    embed.insert("title".into(), JsonValue::String(title_for(event).into()));
    embed.insert("color".into(), JsonValue::from(color_for(event)));
    embed.insert("author".into(), make_author(ctx.actor));
    embed.insert("footer".into(), json!({ "text": ctx.instance_host }));
    embed.insert(
        "timestamp".into(),
        JsonValue::String(ctx.occurred_at.to_rfc3339()),
    );

    if let Some(description) = describe_event(event, ctx) {
        embed.insert("description".into(), JsonValue::String(description));
    }
    if let Some(url) = note_url(ctx.note) {
        embed.insert("url".into(), JsonValue::String(url));
    }

    json!({
        "embeds": [JsonValue::Object(embed)],
    })
}

fn build_plain(event: NotificationEvent, ctx: &NotificationContext<'_>) -> JsonValue {
    let actor_label = actor_acct(ctx.actor);
    let body = describe_event(event, ctx).unwrap_or_default();
    let line = if body.is_empty() {
        format!("[{tag}] {actor_label}", tag = title_for(event))
    } else {
        format!("[{tag}] {actor_label}: {body}", tag = title_for(event))
    };
    // **`allowed_mentions: { parse: [] }`**: Discord 互換 webhook では
    // top-level `content` 内の `@everyone` / `@here` / `<@user_id>` がデフォルトで
    // メンションとして処理される。remote actor が `content: "@everyone ..."` の
    // Note を送ってきた場合に全員 ping が飛ぶのを防ぐため、ping 対象を空集合に
    // 明示する。Slack / Misskey 互換 webhook は本フィールドを無視するので副作用
    // なし。(round-1 review F4)
    json!({
        "content": line,
        "allowed_mentions": { "parse": [] },
    })
}

fn make_author(actor: &ActorRow) -> JsonValue {
    let display = actor
        .display_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&actor.preferred_username);
    let mut author = serde_json::Map::new();
    author.insert("name".into(), JsonValue::String(display.to_string()));
    author.insert("url".into(), JsonValue::String(actor.ap_id.clone()));
    if let Some(icon) = actor.icon_url.clone() {
        author.insert("icon_url".into(), JsonValue::String(icon));
    }
    JsonValue::Object(author)
}

fn actor_acct(actor: &ActorRow) -> String {
    format!(
        "@{user}@{host}",
        user = actor.preferred_username,
        host = actor.host
    )
}

/// Note の `url` フィールド優先、無ければ AP `ap_id`。embed の clickable
/// タイトル用 / plain では使わない。
fn note_url(note: Option<&NoteRow>) -> Option<String> {
    let note = note?;
    if let Some(u) = note.url.clone() {
        return Some(u);
    }
    Some(note.ap_id.clone())
}

/// embed の description / plain の `content` 本文を組み立てる。
fn describe_event(event: NotificationEvent, ctx: &NotificationContext<'_>) -> Option<String> {
    match event {
        NotificationEvent::Mention | NotificationEvent::Direct => ctx
            .note
            .map(|n| note_preview(&n.content, TEXT_PREVIEW_CHARS)),
        NotificationEvent::Quote => {
            // 「引用された自分の note の本文」を先に出し、引用者本文を続ける。
            let theirs = ctx
                .note
                .map(|n| note_preview(&n.content, TEXT_PREVIEW_CHARS / 2));
            let ours = ctx
                .quote_target
                .map(|n| note_preview(&n.content, TEXT_PREVIEW_CHARS / 2));
            match (theirs, ours) {
                (Some(t), Some(o)) => Some(format!("引用元: {o}\n\n{t}")),
                (Some(t), None) => Some(t),
                (None, Some(o)) => Some(format!("引用元: {o}")),
                (None, None) => None,
            }
        }
        NotificationEvent::Reaction => {
            let emoji = ctx.reaction_content.unwrap_or("?");
            let target = ctx
                .note
                .map(|n| note_preview(&n.content, TEXT_PREVIEW_CHARS / 2));
            target.map_or_else(
                || Some(emoji.to_string()),
                |t| Some(format!("{emoji} → {t}")),
            )
        }
        NotificationEvent::Renote => ctx
            .note
            .map(|n| note_preview(&n.content, TEXT_PREVIEW_CHARS)),
        NotificationEvent::Follow | NotificationEvent::FollowRequest => None,
    }
}

/// note 本文 (= AP HTML) を webhook プレビュー用の plain text に倒してから
/// 切り詰める。`note.content` は local / remote とも HTML なので、Discord /
/// Slack 等にそのまま流すと `<p>` / `&lt;` が生で見えてしまう。
/// [`crate::miauth::text::html_to_plain_text`] は副作用の無い純関数なので、
/// 本モジュールの「純粋関数のみ」方針を崩さない。
fn note_preview(content: &str, max: usize) -> String {
    truncate(&crate::miauth::text::html_to_plain_text(content), max)
}

/// `s` を文字 (= Unicode scalar) 単位で `max` 文字に切り詰め、超過時は `…` を
/// 末尾に付ける。byte ベースでは絵文字を割って壊すので `chars()` を使う。
fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sqlx::types::Json;

    fn sample_actor() -> ActorRow {
        ActorRow {
            id: 1,
            ap_id: "https://example.com/users/alice".into(),
            preferred_username: "alice".into(),
            host: "example.com".into(),
            display_name: Some("Alice".into()),
            summary: None,
            icon_url: Some("https://example.com/avatar.png".into()),
            image_url: None,
            inbox_url: "https://example.com/users/alice/inbox".into(),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: "https://example.com/users/alice#main-key".into(),
            public_key_pem: String::new(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: Json(Vec::new()),
            moved_to_ap_id: None,
            is_local: false,
            actor_type: "Person".into(),
            manually_approves_followers: false,
            birthday: None,
            location: None,
            lang: None,
            followed_message: None,
            fields: Json(Vec::new()),
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sample_note(content: &str) -> NoteRow {
        NoteRow {
            id: 1,
            ap_id: "https://example.com/notes/1".into(),
            actor_id: 1,
            content: content.into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: "public".into(),
            sensitive: false,
            to_recipients: Json(Vec::new()),
            cc_recipients: Json(Vec::new()),
            attachments: Json(JsonValue::Array(Vec::new())),
            tags: Json(JsonValue::Array(Vec::new())),
            is_local: false,
            url: Some("https://example.com/@alice/1".into()),
            published_at: Utc::now(),
            edited_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 2, 12, 0, 0).unwrap()
    }

    #[test]
    fn embed_mention_has_color_author_description_and_url() {
        let actor = sample_actor();
        let note = sample_note("こんにちは @me 元気？");
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: Some(&note),
            reaction_content: None,
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Mention, WebhookFormat::Embed, &ctx);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "メンション");
        assert_eq!(embed["color"].as_u64().unwrap(), 0x4F_C3_F7);
        assert_eq!(embed["author"]["name"], "Alice");
        assert_eq!(embed["author"]["url"], "https://example.com/users/alice");
        assert_eq!(
            embed["author"]["icon_url"],
            "https://example.com/avatar.png",
        );
        assert_eq!(embed["footer"]["text"], "sakurasato.example");
        assert_eq!(embed["timestamp"], "2026-06-02T12:00:00+00:00");
        assert!(
            embed["description"]
                .as_str()
                .unwrap()
                .contains("こんにちは")
        );
        assert_eq!(embed["url"], "https://example.com/@alice/1");
    }

    #[test]
    fn embed_reaction_combines_emoji_and_target_preview() {
        let actor = sample_actor();
        let note = sample_note("対象の本文");
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: Some(&note),
            reaction_content: Some(":sparkles:"),
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Reaction, WebhookFormat::Embed, &ctx);
        let desc = payload["embeds"][0]["description"].as_str().unwrap();
        assert!(desc.contains(":sparkles:"));
        assert!(desc.contains("対象の本文"));
        assert_eq!(payload["embeds"][0]["color"].as_u64().unwrap(), 0xFF_D5_4F);
    }

    #[test]
    fn embed_follow_request_has_gray_color_no_description() {
        let actor = sample_actor();
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: None,
            reaction_content: None,
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::FollowRequest, WebhookFormat::Embed, &ctx);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "フォローリクエスト");
        assert_eq!(embed["color"].as_u64().unwrap(), 0x9E_9E_9E);
        assert!(embed.get("description").is_none());
    }

    #[test]
    fn embed_quote_shows_our_note_first_then_their_body() {
        let actor = sample_actor();
        let their_note = sample_note("これはすごい投稿ですね");
        let mut our_note = sample_note("我々の元投稿");
        our_note.id = 99;
        our_note.is_local = true;
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: Some(&their_note),
            reaction_content: None,
            quote_target: Some(&our_note),
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Quote, WebhookFormat::Embed, &ctx);
        let desc = payload["embeds"][0]["description"].as_str().unwrap();
        assert!(desc.starts_with("引用元: 我々の元投稿"));
        assert!(desc.contains("これはすごい投稿ですね"));
    }

    #[test]
    fn plain_format_emits_one_liner_content() {
        let actor = sample_actor();
        let note = sample_note("おはよう");
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: Some(&note),
            reaction_content: None,
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Mention, WebhookFormat::Plain, &ctx);
        assert_eq!(
            payload["content"],
            "[メンション] @alice@example.com: おはよう"
        );
        assert!(payload.get("embeds").is_none());
        // `allowed_mentions.parse = []` (= ping 対象なし) が常に付くこと。
        // Discord 以外の webhook は無視するので副作用なし。
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }

    /// remote が `@everyone` を含む Note を送ってきても Discord 全員 ping が
    /// 発火しないこと (round-1 review F4)。`content` には文字列としてそのまま
    /// 入るが、`allowed_mentions.parse: []` で実 ping は無効化される。
    #[test]
    fn plain_format_suppresses_everyone_ping() {
        let actor = sample_actor();
        let note = sample_note("@everyone @here urgent");
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: Some(&note),
            reaction_content: None,
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Mention, WebhookFormat::Plain, &ctx);
        assert!(
            payload["content"]
                .as_str()
                .unwrap()
                .contains("@everyone @here urgent")
        );
        assert_eq!(payload["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn plain_format_omits_body_for_follow_event() {
        let actor = sample_actor();
        let ctx = NotificationContext {
            actor: &actor,
            instance_host: "sakurasato.example",
            note: None,
            reaction_content: None,
            quote_target: None,
            occurred_at: fixed_time(),
        };
        let payload = build_payload(NotificationEvent::Follow, WebhookFormat::Plain, &ctx);
        assert_eq!(payload["content"], "[フォロー] @alice@example.com");
    }

    #[test]
    fn truncate_keeps_multibyte_codepoints_whole() {
        // 5 文字目以降を切る場合、絵文字 (= 4 byte 1 codepoint) を割らない。
        let s = "あいうえお🐱🐶";
        let out = truncate(s, 5);
        // 5 文字 + `…`。`🐱` 以降は省略される。
        assert_eq!(out, "あいうえお…");
    }

    #[test]
    fn webhook_format_round_trips_through_db_string() {
        assert_eq!(WebhookFormat::from_db("embed").as_str(), "embed");
        assert_eq!(WebhookFormat::from_db("plain").as_str(), "plain");
        // 既知でない値は embed フォールバック。
        assert_eq!(WebhookFormat::from_db("???").as_str(), "embed");
    }
}
