//! Compile-time checked queries against the `media` table (M7).

use sqlx::PgPool;

use crate::model::MediaRow;

#[derive(Debug, Clone)]
pub struct NewMedia {
    pub storage_key: String,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub byte_size: i64,
    /// 'avatar' / 'header' / 'attachment' のいずれか。`CHECK` 制約で縛る。
    pub kind: String,
    pub alt_text: Option<String>,
    pub owner_actor_id: i64,
}

/// 行を挿入して返す。`note_id` は常に NULL で入る (= 添付紐付けは別 API)。
pub async fn insert<'e, E>(executor: E, new: NewMedia) -> sqlx::Result<MediaRow>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        MediaRow,
        r#"
        INSERT INTO media (
            storage_key, media_type, width, height, byte_size,
            kind, alt_text, owner_actor_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING
            id, storage_key, media_type, width, height, byte_size,
            kind, alt_text, owner_actor_id, note_id, created_at, updated_at
        "#,
        new.storage_key,
        new.media_type,
        new.width,
        new.height,
        new.byte_size,
        new.kind,
        new.alt_text,
        new.owner_actor_id,
    )
    .fetch_one(executor)
    .await
}

/// 同じ `storage_key` を持つ既存行を返す。重複アップロード時の dedupe に使う
/// (= 同じバイト列のサニタイズ結果は同じ SHA-256 = 同じ `storage_key` になる)。
pub async fn get_by_storage_key(
    pool: &PgPool,
    storage_key: &str,
) -> sqlx::Result<Option<MediaRow>> {
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at
        FROM media WHERE storage_key = $1
        "#,
        storage_key,
    )
    .fetch_optional(pool)
    .await
}

/// `id` で 1 行引く。
pub async fn get_by_id<'e, E>(executor: E, id: i64) -> sqlx::Result<Option<MediaRow>>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at
        FROM media WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(executor)
    .await
}

/// `ids` を一括取得する。順序は入力 `ids` 順ではなく id 昇順で返す
/// (`note.attachments` JSONB に書き込む前に呼び出し側が並べ替える前提)。
///
/// `ids` が空のときは即座に空 Vec を返す ── 空配列を $1 に渡すと
/// `PostgreSQL` の `= ANY` が型推論できずエラーになるため。
pub async fn list_by_ids(pool: &PgPool, ids: &[i64]) -> sqlx::Result<Vec<MediaRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at
        FROM media WHERE id = ANY($1)
        ORDER BY id ASC
        "#,
        ids,
    )
    .fetch_all(pool)
    .await
}

/// 添付として `note_id` に紐付ける。POST /api/v1/notes が note 挿入直後の
/// 同一 tx 内で呼ぶ。`ids` の各行が `owner_actor_id == actor_id` かつ
/// `note_id IS NULL` であることをここで保証する (= 横取り防止)。
///
/// 返り値は実際に紐付いた行数。`ids.len()` と一致しない場合は所有者違反
/// または二重紐付け試行があるので、呼び出し側は失敗扱いにする。
pub async fn attach_to_note<'e, E>(
    executor: E,
    ids: &[i64],
    actor_id: i64,
    note_id: i64,
) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query!(
        r#"
        UPDATE media
        SET note_id = $1, updated_at = now()
        WHERE id = ANY($2)
          AND owner_actor_id = $3
          AND note_id IS NULL
        "#,
        note_id,
        ids,
        actor_id,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}
