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
