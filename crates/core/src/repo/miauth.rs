//! Compile-time checked queries against the `miauth_token` / `miauth_session`
//! tables (= M14 #157, 親 issue #150 `MiAuth` foundation)。
//!
//! 既存 [`crate::repo::api_token`] (= TUI 用 Bearer, permission 概念なし) と
//! 完全分離。auth middleware は別経路 ── 本 repo を経由するのは `MiAuth` 用
//! handler だけで、TUI 用 token とテーブルレベルで一切干渉しない。
//!
//! ## permission scope の扱い
//!
//! `Vec<String>` (= `["read:account", "write:reactions"]`) を `sqlx::types::Json`
//! 包みで JSONB 列に書き込む。`query_as!` で列を取り出すときは
//! `permissions as "permissions: Json<Vec<String>>"` の alias 必須 (= sqlx 0.9
//! 流儀、CLAUDE.md §10 と memory `[[sqlx-09-workflow]]` 参照)。
//!
//! ## state 遷移の CAS
//!
//! `approve_session` / `reject_session` / `mark_session_consumed` / `expire_old_sessions`
//! はすべて **「現状 state が期待値のときだけ書き換える」CAS パターン** で実装する
//! (= UPDATE ... WHERE state = '...')。`rows_affected()` を返すので、呼び出し側
//! は 0 か 1 で「遷移できたか」を判定する。並行 approve/reject の race も DB
//! 制約だけで安全に解決できる。

#![allow(clippy::too_many_arguments)]

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::types::Json;
use uuid::Uuid;

use crate::model::{MiAuthSessionRow, MiAuthTokenRow};

// ── miauth_token ─────────────────────────────────────────────────────

/// Fields required to insert a new `MiAuth` token. `created_at` is `DEFAULT now()`
/// and `last_used_at` starts NULL.
#[derive(Clone)]
pub struct NewMiAuthToken {
    pub name: String,
    /// SHA-256 of raw token (`Base64URL` no-pad)。生 token は server crate の
    /// `miauth::token::hash` で計算する ── 本 repo に raw token は来ない。
    pub token_hash: String,
    /// permission scope の文字列 list (`["read:account", "write:reactions"]`)。
    /// approve 時に snapshot した permissions と同一。
    pub permissions: Vec<String>,
}

/// `Debug` redacts `token_hash` to mirror [`MiAuthTokenRow`]'s policy.
impl std::fmt::Debug for NewMiAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewMiAuthToken")
            .field("name", &self.name)
            .field("token_hash", &"<redacted>")
            .field("permissions", &self.permissions)
            .finish()
    }
}

pub async fn insert_token(pool: &PgPool, new: NewMiAuthToken) -> sqlx::Result<MiAuthTokenRow> {
    let permissions_json =
        serde_json::to_value(&new.permissions).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        MiAuthTokenRow,
        r#"
        INSERT INTO miauth_token (name, token_hash, permissions)
        VALUES ($1, $2, $3)
        RETURNING
            id, name, token_hash,
            permissions as "permissions: Json<Vec<String>>",
            last_used_at, created_at
        "#,
        new.name,
        new.token_hash,
        permissions_json,
    )
    .fetch_one(pool)
    .await
}

/// Look up a `MiAuth` token by its hash. Used by `miauth::auth::require_token`.
/// Returns `None` for unknown / revoked hashes.
pub async fn find_token_by_hash(
    pool: &PgPool,
    token_hash: &str,
) -> sqlx::Result<Option<MiAuthTokenRow>> {
    sqlx::query_as!(
        MiAuthTokenRow,
        r#"
        SELECT
            id, name, token_hash,
            permissions as "permissions: Json<Vec<String>>",
            last_used_at, created_at
        FROM miauth_token
        WHERE token_hash = $1
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await
}

/// Look up the token issued for a given session (= `consumed` state). Used by
/// `POST /api/miauth/{uuid}/check` to make repeated polls **idempotent** ──
/// the client may keep polling after a transient network blip, and we must
/// return the same token without issuing a new row.
pub async fn find_token_by_session(
    pool: &PgPool,
    uuid: Uuid,
) -> sqlx::Result<Option<MiAuthTokenRow>> {
    sqlx::query_as!(
        MiAuthTokenRow,
        r#"
        SELECT
            t.id, t.name, t.token_hash,
            t.permissions as "permissions: Json<Vec<String>>",
            t.last_used_at, t.created_at
        FROM miauth_token t
        JOIN miauth_session s ON s.issued_token_id = t.id
        WHERE s.uuid = $1
        "#,
        uuid,
    )
    .fetch_optional(pool)
    .await
}

/// `last_used_at = now()` stamp. Best-effort: the auth middleware spawns this
/// as a detached task so request handling isn't blocked when the UPDATE fails.
pub async fn mark_token_used(pool: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE miauth_token SET last_used_at = now() WHERE id = $1"#,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// List all `MiAuth` tokens (CLI `miauth list`).
pub async fn list_tokens(pool: &PgPool) -> sqlx::Result<Vec<MiAuthTokenRow>> {
    sqlx::query_as!(
        MiAuthTokenRow,
        r#"
        SELECT
            id, name, token_hash,
            permissions as "permissions: Json<Vec<String>>",
            last_used_at, created_at
        FROM miauth_token
        ORDER BY created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Hard-delete a `MiAuth` token (CLI `miauth revoke --id N`). Returns `true`
/// when a row was removed. Associated session rows have `issued_token_id`
/// set to NULL by FK `ON DELETE SET NULL` so the audit trail (= which session
/// produced which now-revoked token) survives.
pub async fn delete_token_by_id(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query!(r#"DELETE FROM miauth_token WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ── miauth_session ───────────────────────────────────────────────────

/// Fields required to insert a new pending `MiAuth` session (= browser landing
/// `GET /miauth/{uuid}?name=&permission=&callback=`).
#[derive(Debug, Clone)]
pub struct NewMiAuthSession {
    pub uuid: Uuid,
    pub app_name: String,
    pub callback_url: Option<String>,
    /// Browser landing 時点で client が要求した permission scope。CLI で
    /// approve するときに `--permission` で書き換え可 (= ユーザがクライアント
    /// 要求より絞れる)。snapshot は approve 時の値で固定。
    pub permissions: Vec<String>,
    /// `requested_at + session_ttl_secs` を呼び出し側で算出して渡す。
    pub expires_at: DateTime<Utc>,
}

pub async fn insert_session(
    pool: &PgPool,
    new: NewMiAuthSession,
) -> sqlx::Result<MiAuthSessionRow> {
    let permissions_json =
        serde_json::to_value(&new.permissions).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        MiAuthSessionRow,
        r#"
        INSERT INTO miauth_session (uuid, app_name, callback_url, permissions, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            uuid, app_name, callback_url,
            permissions as "permissions: Json<Vec<String>>",
            state, issued_token_id, requested_at, approved_at, expires_at,
            raw_token_for_polling
        "#,
        new.uuid,
        new.app_name,
        new.callback_url,
        permissions_json,
        new.expires_at,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_session(pool: &PgPool, uuid: Uuid) -> sqlx::Result<Option<MiAuthSessionRow>> {
    sqlx::query_as!(
        MiAuthSessionRow,
        r#"
        SELECT
            uuid, app_name, callback_url,
            permissions as "permissions: Json<Vec<String>>",
            state, issued_token_id, requested_at, approved_at, expires_at,
            raw_token_for_polling
        FROM miauth_session
        WHERE uuid = $1
        "#,
        uuid,
    )
    .fetch_optional(pool)
    .await
}

/// **CAS**: pending session を approved に倒し、permission snapshot を上書き
/// する。`rows_affected = 1` なら遷移成功、`0` なら「pending では無かった」
/// (= 既に approved / rejected / consumed / expired / 存在しない)。
///
/// 呼び出し側 (CLI `miauth approve` / `POST /api/miauth/{uuid}/check` を待つ
/// クライアントが叩く前段) は 0 のとき [`get_session`] で現状を再確認して
/// ユーザに「既に承認済」「拒否済」等を表示する。
pub async fn approve_session(
    pool: &PgPool,
    uuid: Uuid,
    permissions: &[String],
) -> sqlx::Result<u64> {
    let permissions_json =
        serde_json::to_value(permissions).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let res = sqlx::query!(
        r#"
        UPDATE miauth_session
        SET state = 'approved',
            permissions = $2,
            approved_at = now()
        WHERE uuid = $1
          AND state = 'pending'
        "#,
        uuid,
        permissions_json,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// **CAS**: pending session を rejected に倒す。
pub async fn reject_session(pool: &PgPool, uuid: Uuid) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        r#"
        UPDATE miauth_session
        SET state = 'rejected'
        WHERE uuid = $1
          AND state = 'pending'
        "#,
        uuid,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// **CAS**: approved session を consumed に倒し、発行した token id を結びつける。
/// 呼び出し側 (`POST /api/miauth/{uuid}/check`) は先に [`insert_token`] で
/// token を発行し、その id を渡す。
///
/// `0` を返したら「既に consumed (= 別 polling が先に走った)」または「state
/// が approved ではない」のどちらか。前者は [`find_token_by_session`] で既存
/// token を返せばよく (冪等)、後者はエラー応答にする。
pub async fn mark_session_consumed(pool: &PgPool, uuid: Uuid, token_id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        r#"
        UPDATE miauth_session
        SET state = 'consumed',
            issued_token_id = $2
        WHERE uuid = $1
          AND state = 'approved'
        "#,
        uuid,
        token_id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// **M14 #158**: approved session を consumed に倒し、token id + raw token を
/// 同時に書き込む CAS。`mark_session_consumed` と違い `raw_token_for_polling`
/// 列にも値を入れるので、`POST /api/miauth/{uuid}/check` の冪等 (= 2 回目以降
/// で同じ raw token を返す) を実現する。
///
/// `0` を返したら別 polling が先に CAS を成功させた競合 ── 呼び出し側は
/// `get_session` で raw を読み直して同じ token を返す経路に倒す。
pub async fn mark_session_consumed_with_raw(
    pool: &PgPool,
    uuid: Uuid,
    token_id: i64,
    raw_token: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        r#"
        UPDATE miauth_session
        SET state = 'consumed',
            issued_token_id = $2,
            raw_token_for_polling = $3
        WHERE uuid = $1
          AND state = 'approved'
        "#,
        uuid,
        token_id,
        raw_token,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// `state = 'pending' AND now() > expires_at` の行を `expired` に倒す。
/// 戻り値は遷移した行数。
///
/// 呼び出し頻度: foundation #157 では CLI `miauth list` / handler の各種
/// 経路で best-effort 起動を想定 (= 専用 GC タスクは持たず、自然な経路で
/// 漸進的に sweep する)。`miauth.session_ttl_secs` が極端に短くなければ
/// この方針で十分。
pub async fn expire_old_sessions(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        r#"
        UPDATE miauth_session
        SET state = 'expired'
        WHERE state = 'pending'
          AND now() > expires_at
        "#,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// `state = 'pending'` の session を `requested_at` 古い順で列挙する
/// (CLI `miauth list` 用)。期限切れは内側で先に sweep してから返す
/// (= ユーザが「もう切れている session」を approve できないように)。
pub async fn list_pending_sessions(pool: &PgPool) -> sqlx::Result<Vec<MiAuthSessionRow>> {
    // best-effort sweep (失敗しても list は試みる ── pool exhaustion などで
    // sweep が落ちても CLI 表示は通るほうが嬉しい)。
    let _ = expire_old_sessions(pool).await;
    sqlx::query_as!(
        MiAuthSessionRow,
        r#"
        SELECT
            uuid, app_name, callback_url,
            permissions as "permissions: Json<Vec<String>>",
            state, issued_token_id, requested_at, approved_at, expires_at,
            raw_token_for_polling
        FROM miauth_session
        WHERE state = 'pending'
        ORDER BY requested_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
}
