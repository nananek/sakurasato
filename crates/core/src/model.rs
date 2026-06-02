//! Database row types for the M2 schema.
//!
//! Each struct mirrors a table 1:1 and derives [`sqlx::FromRow`] so it can be
//! plugged into `query_as!` / `query_as` calls. The actual queries live in
//! [`crate::repo`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx::types::Json;

/// Row of the `actor` table.
///
/// `private_key_pem` is intentionally excluded from `Serialize` and redacted
/// in `Debug` so the local actor's signing key cannot leak via the local
/// API (M4) or `tracing::debug!(?actor)` calls. Mirrors the same protection
/// applied to [`crate::config::StorageConfig::secret_access_key`].
#[derive(Clone, FromRow, Serialize, Deserialize)]
pub struct ActorRow {
    pub id: i64,
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub icon_url: Option<String>,
    pub image_url: Option<String>,
    pub inbox_url: String,
    pub shared_inbox_url: Option<String>,
    pub outbox_url: Option<String>,
    pub followers_url: Option<String>,
    pub following_url: Option<String>,
    pub public_key_id: String,
    pub public_key_pem: String,
    /// `#[serde(skip)]` で双方向を遮断する: M4 以降の API レイヤで
    /// `serde_json::from_value::<ActorRow>(untrusted_json)` 経由で外部 JSON の
    /// `private_key_pem` が取り込まれるマスアサインメント脆弱性を防ぐ。
    /// 永続化からの復元は `sqlx::FromRow` が担うので serde を経由する必要はない。
    #[serde(skip)]
    pub private_key_pem: Option<String>,
    /// Ed25519 公開鍵の `ActivityPub` キー ID (FEP-521a `assertionMethod` の
    /// `id`)。RSA 側 [`Self::public_key_id`] とは別物。Ed25519 鍵を持たない
    /// local actor (`init --force` 未実行の旧 actor) や、`publicKey` に RSA
    /// しか公開していない remote actor では `None`。
    pub ed25519_public_key_id: Option<String>,
    /// Ed25519 公開鍵 (PKCS#8 SPKI PEM)。
    pub ed25519_public_key_pem: Option<String>,
    /// Ed25519 秘密鍵 (PKCS#8 PEM)。RSA 側と同じく
    /// マスアサインメント脆弱性回避のため `#[serde(skip)]`。
    #[serde(skip)]
    pub ed25519_private_key_pem: Option<String>,
    pub also_known_as: Json<Vec<String>>,
    pub moved_to_ap_id: Option<String>,
    pub is_local: bool,
    pub actor_type: String,
    /// 鍵アカ運用フラグ (Issue #66 / M12)。
    ///
    /// `true` のとき、本 actor 宛の inbound `Follow` は auto-Accept されず
    /// `follow.state = pending` で据え置かれる。承認は管理 CLI
    /// (`sakurasato-server follow-request approve/reject`) で明示的に行う。
    ///
    /// remote actor 行にもこの値を載せるが、本サーバの dispatcher が分岐に
    /// 使うのは local actor (= 我々自身) の値のみ ── 相手側インスタンスが
    /// 自分の `manuallyApprovesFollowers` をどう扱うかは我々の管轄外。
    /// remote 側はキャッシュとして保持しておくに留める。
    pub manually_approves_followers: bool,
    pub fetched_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for ActorRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRow")
            .field("id", &self.id)
            .field("ap_id", &self.ap_id)
            .field("preferred_username", &self.preferred_username)
            .field("host", &self.host)
            .field("display_name", &self.display_name)
            .field("summary", &self.summary)
            .field("icon_url", &self.icon_url)
            .field("image_url", &self.image_url)
            .field("inbox_url", &self.inbox_url)
            .field("shared_inbox_url", &self.shared_inbox_url)
            .field("outbox_url", &self.outbox_url)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("public_key_id", &self.public_key_id)
            .field("public_key_pem", &self.public_key_pem)
            .field(
                "private_key_pem",
                &self.private_key_pem.as_ref().map(|_| "<redacted>"),
            )
            .field("ed25519_public_key_id", &self.ed25519_public_key_id)
            .field("ed25519_public_key_pem", &self.ed25519_public_key_pem)
            .field(
                "ed25519_private_key_pem",
                &self.ed25519_private_key_pem.as_ref().map(|_| "<redacted>"),
            )
            .field("also_known_as", &self.also_known_as)
            .field("moved_to_ap_id", &self.moved_to_ap_id)
            .field("is_local", &self.is_local)
            .field("actor_type", &self.actor_type)
            .field(
                "manually_approves_followers",
                &self.manually_approves_followers,
            )
            .field("fetched_at", &self.fetched_at)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Row of the `note` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct NoteRow {
    pub id: i64,
    pub ap_id: String,
    pub actor_id: i64,
    pub content: String,
    pub language: Option<String>,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub to_recipients: Json<Vec<String>>,
    pub cc_recipients: Json<Vec<String>>,
    pub attachments: Json<serde_json::Value>,
    pub tags: Json<serde_json::Value>,
    pub is_local: bool,
    pub url: Option<String>,
    pub published_at: DateTime<Utc>,
    /// M11: リモート `Update`/`Note` を受領した時刻 (= 編集時刻)。
    /// 初回受信時は `None`。ローカル投稿の編集機能は未実装なので、現状は
    /// inbound 専用のメタデータ。`updated_at` は他の更新でも動くので別列。
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row of the `follow` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct FollowRow {
    pub id: i64,
    pub ap_id: String,
    pub follower_actor_id: i64,
    pub followed_actor_id: i64,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row of the `delivery_queue` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct DeliveryQueueRow {
    pub id: i64,
    pub inbox_url: String,
    pub activity: Json<serde_json::Value>,
    pub sender_actor_id: i64,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row of the `emoji` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct EmojiRow {
    pub id: i64,
    pub shortcode: String,
    pub host: Option<String>,
    pub category: Option<String>,
    pub aliases: Json<Vec<String>>,
    pub image_key: String,
    pub media_type: String,
    pub ap_id: Option<String>,
    pub is_local: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row of the `announce` table (M11).
///
/// リモート actor の Boost (`Announce`) を記録する。`(note_id, actor_id)`
/// UNIQUE で二重 boost は idempotent。`Undo` で `ap_id` を引いて削除する。
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AnnounceRow {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub actor_id: i64,
    pub published_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Row of the `reaction` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ReactionRow {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub actor_id: i64,
    pub content: String,
    pub emoji_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// Row of the `api_token` table (M4 PR1).
///
/// `token_hash` is the SHA-256 hex of the raw token; the raw value is never
/// stored. The hash is itself sensitive enough — anyone with read access to
/// this column could replay the token if they also obtained the original
/// value — so we omit it from `Serialize` (defence-in-depth against
/// accidentally rendering an `ApiTokenRow` through the local API) and
/// redact it in `Debug` so `tracing::debug!(?row)` calls cannot leak it.
#[derive(Clone, FromRow, Deserialize)]
pub struct ApiTokenRow {
    pub id: i64,
    pub name: String,
    /// `#[serde(skip)]` mirrors the protection applied to
    /// [`ActorRow::private_key_pem`]: prevents external JSON from injecting
    /// a `token_hash` via `serde_json::from_value::<ApiTokenRow>(...)` and
    /// prevents accidental rendering through the local API.
    #[serde(skip)]
    pub token_hash: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl std::fmt::Debug for ApiTokenRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiTokenRow")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("token_hash", &"<redacted>")
            .field("last_used_at", &self.last_used_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Row of the `media` table (M7).
///
/// `media-proxy` でサニタイズ後に versitygw へ格納された画像メタデータ。
/// `storage_key` は `media/<sha256>.webp` 形式で、`GET /media/{key}` に
/// そのまま渡る (`routes::media::handle`)。
///
/// `kind` は 'avatar' / 'header' / 'attachment' のいずれか (`CHECK` 制約)。
/// `note_id` は添付として紐付いた Note。NULL は未紐付け。
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaRow {
    pub id: i64,
    pub storage_key: String,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub byte_size: i64,
    pub kind: String,
    pub alt_text: Option<String>,
    pub owner_actor_id: i64,
    pub note_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Visibility enum (mirrors the `note.visibility` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Unlisted,
    Followers,
    Direct,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Followers => "followers",
            Self::Direct => "direct",
        }
    }
}

/// Follow-relationship state (mirrors `follow.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowState {
    Pending,
    Accepted,
    Rejected,
}

impl FollowState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// Row of the `notification_channel` table.
///
/// Discord 互換 webhook の宛先 1 件。`url` が漏れると第三者が同じチャンネルに
/// 任意メッセージを撃てる (= なりすまし) ので、`Serialize` に乗せても問題ない
/// 範囲か呼び出し側で見ること。現状は管理 CLI 経由でしか読み書きされない。
//
// clippy::struct_excessive_bools: 7 個の `notify_*` フラグは「通知 event 種別」
// を 1:1 で列挙する DB 列のミラーであり、enum / state machine に置き換える
// のは不適切 (= 1 channel が複数 event を独立に on/off できる必要があり、
// `HashSet<NotificationEvent>` 形式に倒すと正規化が壊れる)。本構造体は repo
// 層の型として閉じているのでフィールドアクセスが分散しない。
//
// **master `enabled` 列は migration 0014 で撤去された** (元は 2 段スイッチに
// していたが `--event all` の直感とぶつかり、`toggle` が冪等にならなかった)。
// チャンネル全停止は `disable --event all` で 7 個 `notify_*` を一斉 FALSE に
// する運用に統一されている。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct NotificationChannelRow {
    pub id: i64,
    pub name: String,
    pub url: String,
    /// `embed` (Discord embed JSON) または `plain` (`{"content": "..."}` の
    /// Slack / Misskey fallback)。`CHECK` 制約付きなので不正値は入らない。
    pub format: String,
    pub notify_mention: bool,
    pub notify_direct: bool,
    pub notify_quote: bool,
    pub notify_reaction: bool,
    pub notify_renote: bool,
    pub notify_follow: bool,
    pub notify_follow_request: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 通知イベント種別。`notification_channel.notify_<event>` 列と 1:1 対応し、
/// `delivery_queue.activity` JSONB の `"event"` フィールドに `as_str` の値で
/// 埋め込む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEvent {
    Mention,
    Direct,
    Quote,
    Reaction,
    Renote,
    Follow,
    FollowRequest,
}

impl NotificationEvent {
    /// `delivery_queue.activity.event` で使う wire 表現。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Direct => "direct",
            Self::Quote => "quote",
            Self::Reaction => "reaction",
            Self::Renote => "renote",
            Self::Follow => "follow",
            Self::FollowRequest => "follow_request",
        }
    }

    /// 対応する `notification_channel.notify_*` 列名。`list_enabled_for_event`
    /// の WHERE 句に動的列名で使うために露出する。**ユーザ入力に対しては絶対
    /// に使わないこと** (`from_str` で enum に倒してから本関数を経由する経路
    /// にする ── enum 値は静的なので SQL injection の入口にならない)。
    pub fn column_name(self) -> &'static str {
        match self {
            Self::Mention => "notify_mention",
            Self::Direct => "notify_direct",
            Self::Quote => "notify_quote",
            Self::Reaction => "notify_reaction",
            Self::Renote => "notify_renote",
            Self::Follow => "notify_follow",
            Self::FollowRequest => "notify_follow_request",
        }
    }

    /// 7 全 variant の配列 (CLI の `toggle --event all` 経路で iterate する用)。
    pub fn all() -> &'static [Self] {
        &[
            Self::Mention,
            Self::Direct,
            Self::Quote,
            Self::Reaction,
            Self::Renote,
            Self::Follow,
            Self::FollowRequest,
        ]
    }
}

impl std::str::FromStr for NotificationEvent {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mention" => Ok(Self::Mention),
            "direct" => Ok(Self::Direct),
            "quote" => Ok(Self::Quote),
            "reaction" => Ok(Self::Reaction),
            "renote" => Ok(Self::Renote),
            "follow" => Ok(Self::Follow),
            // CLI は kebab-case / snake_case 両受け (clap が `--event follow-request` を渡してくる)。
            "follow_request" | "follow-request" => Ok(Self::FollowRequest),
            other => Err(format!(
                "unknown notification event {other:?}; expected one of mention|direct|quote|reaction|renote|follow|follow-request"
            )),
        }
    }
}

/// Delivery-queue state (mirrors `delivery_queue.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed,
    Dead,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}
