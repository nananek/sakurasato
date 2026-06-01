//! Compile-time checked queries against the `actor` table.

use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::ActorRow;

/// Fields required to insert a new actor row. Database-managed columns
/// (`id`, `created_at`, `updated_at`) are not part of this struct.
///
/// Custom `Debug` redacts `private_key_pem`; deriving `Debug` would leak the
/// signing key into any `tracing::debug!(?new_actor)` call.
#[derive(Clone)]
pub struct NewActor {
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
    pub private_key_pem: Option<String>,
    /// Ed25519 公開鍵 ID (典型的には `<ap_id>#ed25519-key`)。RSA とのデュア
    /// ル鍵運用のため [`Self::public_key_id`] と並べて保持する。Ed25519 鍵を
    /// 持たない remote actor では `None`。
    pub ed25519_public_key_id: Option<String>,
    pub ed25519_public_key_pem: Option<String>,
    pub ed25519_private_key_pem: Option<String>,
    pub also_known_as: Vec<String>,
    pub moved_to_ap_id: Option<String>,
    pub is_local: bool,
    pub actor_type: String,
    /// 鍵アカフラグ (Issue #66 / M12)。`true` のとき inbound `Follow` は
    /// auto-Accept されず `follow.state = pending` で据え置かれる。
    /// remote actor を upsert する際は相手側 actor JSON の
    /// `manuallyApprovesFollowers` をキャッシュとして書く。
    pub manually_approves_followers: bool,
}

impl std::fmt::Debug for NewActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewActor")
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
            .finish()
    }
}

/// Insert a new actor and return the persisted row.
///
/// Generic over the executor so callers can pass either a `&PgPool`
/// (default) or a `&mut sqlx::PgConnection` / `&mut Transaction` when the
/// insert needs to share a transaction with surrounding operations.
pub async fn insert<'e, E>(executor: E, new: NewActor) -> sqlx::Result<ActorRow>
where
    E: sqlx::PgExecutor<'e>,
{
    let also_known_as_json =
        serde_json::to_value(&new.also_known_as).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        ActorRow,
        r#"
        INSERT INTO actor (
            ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as, moved_to_ap_id, is_local, actor_type,
            manually_approves_followers
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23
        )
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        "#,
        new.ap_id,
        new.preferred_username,
        new.host,
        new.display_name,
        new.summary,
        new.icon_url,
        new.image_url,
        new.inbox_url,
        new.shared_inbox_url,
        new.outbox_url,
        new.followers_url,
        new.following_url,
        new.public_key_id,
        new.public_key_pem,
        new.private_key_pem,
        new.ed25519_public_key_id,
        new.ed25519_public_key_pem,
        new.ed25519_private_key_pem,
        also_known_as_json,
        new.moved_to_ap_id,
        new.is_local,
        new.actor_type,
        new.manually_approves_followers,
    )
    .fetch_one(executor)
    .await
}

/// Look an actor up by its `id` primary key.
pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        FROM actor WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// すべての local actor (= `is_local = true`) を列挙する。
///
/// お一人様 server では通常 1 行だが、`init --force` で `[server].user` が
/// 変更された場合に **旧 actor を取りこぼさず削除** するため使う
/// (Issue #73)。
pub async fn list_local(pool: &PgPool) -> sqlx::Result<Vec<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        FROM actor WHERE is_local = TRUE
        ORDER BY id ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Look an actor up by its `ap_id` (canonical `ActivityPub` URI).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        FROM actor WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Look an actor up by the (`preferred_username`, `host`) pair.
/// Useful for `WebFinger` lookups.
pub async fn get_by_username_host(
    pool: &PgPool,
    preferred_username: &str,
    host: &str,
) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        FROM actor WHERE preferred_username = $1 AND host = $2
        "#,
        preferred_username,
        host,
    )
    .fetch_optional(pool)
    .await
}

/// Touch the `updated_at` column and refresh `fetched_at` for a remote actor.
/// Used when re-fetching actor metadata from a remote server.
pub async fn mark_fetched(pool: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE actor SET fetched_at = now(), updated_at = now() WHERE id = $1",
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// Update the editable profile fields of a local actor (M7).
///
/// `display_name` / `summary` / `icon_url` / `image_url` のうち `Some` を
/// 渡したフィールドだけ書き換える。`None` は「触らない」を意味する
/// (= NULL を入れたいときは `Some(None)` を渡す ─ そのため Option<Option<...>>)。
///
/// 返り値は更新後の actor 行。
pub async fn update_profile(
    pool: &PgPool,
    id: i64,
    display_name: Option<Option<String>>,
    summary: Option<Option<String>>,
    icon_url: Option<Option<String>>,
    image_url: Option<Option<String>>,
) -> sqlx::Result<crate::model::ActorRow> {
    sqlx::query_as!(
        crate::model::ActorRow,
        r#"
        UPDATE actor SET
            display_name = CASE WHEN $2::BOOLEAN THEN $3 ELSE display_name END,
            summary      = CASE WHEN $4::BOOLEAN THEN $5 ELSE summary      END,
            icon_url     = CASE WHEN $6::BOOLEAN THEN $7 ELSE icon_url     END,
            image_url    = CASE WHEN $8::BOOLEAN THEN $9 ELSE image_url    END,
            updated_at   = now()
        WHERE id = $1
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        "#,
        id,
        display_name.is_some(),
        display_name.flatten(),
        summary.is_some(),
        summary.flatten(),
        icon_url.is_some(),
        icon_url.flatten(),
        image_url.is_some(),
        image_url.flatten(),
    )
    .fetch_one(pool)
    .await
}

/// Replace the `also_known_as` array for an actor (M9 alias CLI / Move 受領).
///
/// `aliases` を **そのまま** 上書きするので、追加/削除は呼び出し側で配列を
/// 整えること。`also_known_as` は JSONB 配列で重複検査は無いが、
/// 慣習として URI 文字列のみが入る。
pub async fn set_also_known_as(
    pool: &PgPool,
    id: i64,
    aliases: &[String],
) -> sqlx::Result<crate::model::ActorRow> {
    let value = serde_json::to_value(aliases).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        crate::model::ActorRow,
        r#"
        UPDATE actor SET
            also_known_as = $2,
            updated_at = now()
        WHERE id = $1
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        "#,
        id,
        value,
    )
    .fetch_one(pool)
    .await
}

/// Set or clear `moved_to_ap_id` for an actor (M9 Move 受領 / 送出)。
///
/// `target` が `Some(uri)` なら `movedTo = uri`、`None` なら NULL に倒す
/// (= Move を取り消したい / 誤入力からの復旧)。送出側は CLI で立て、受領側は
/// inbox の Move handler で立てる。Move を立てると actor JSON の `movedTo`
/// が出るので、相手が actor を再 fetch すれば自然に新しい先へ案内できる。
pub async fn set_moved_to(
    pool: &PgPool,
    id: i64,
    target: Option<&str>,
) -> sqlx::Result<crate::model::ActorRow> {
    sqlx::query_as!(
        crate::model::ActorRow,
        r#"
        UPDATE actor SET
            moved_to_ap_id = $2,
            updated_at = now()
        WHERE id = $1
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        "#,
        id,
        target,
    )
    .fetch_one(pool)
    .await
}

/// Flip `manually_approves_followers` for an actor (Issue #66 / M12).
///
/// `lock` / `unlock` CLI と `init --locked` から呼ばれる。フラグが変わると
/// actor JSON の `manuallyApprovesFollowers` が変わるので、呼び出し側で
/// `Update` activity をフォロワーに配信する責務がある。
pub async fn set_manually_approves_followers(
    pool: &PgPool,
    id: i64,
    locked: bool,
) -> sqlx::Result<crate::model::ActorRow> {
    sqlx::query_as!(
        crate::model::ActorRow,
        r#"
        UPDATE actor SET
            manually_approves_followers = $2,
            updated_at = now()
        WHERE id = $1
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            fetched_at, created_at, updated_at
        "#,
        id,
        locked,
    )
    .fetch_one(pool)
    .await
}

/// Delete an actor by primary key. Used by the `init --force` admin path
/// when re-issuing the local signing key (notes/follows cascade).
pub async fn delete_by_id<'e, E>(executor: E, id: i64) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query!("DELETE FROM actor WHERE id = $1", id)
        .execute(executor)
        .await?
        .rows_affected())
}
