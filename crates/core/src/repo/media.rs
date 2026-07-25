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
    /// 動画の再生時間 (ミリ秒)。画像は常に `None`。
    pub duration_ms: Option<i64>,
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
            kind, alt_text, owner_actor_id, duration_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING
            id, storage_key, media_type, width, height, byte_size,
            kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        "#,
        new.storage_key,
        new.media_type,
        new.width,
        new.height,
        new.byte_size,
        new.kind,
        new.alt_text,
        new.owner_actor_id,
        new.duration_ms,
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
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
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
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
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
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        FROM media WHERE id = ANY($1)
        ORDER BY id ASC
        "#,
        ids,
    )
    .fetch_all(pool)
    .await
}

/// 指定 `note_id` に紐付いた `media` 行を全件返す。`id ASC` 順 ──
/// `attach_to_note` は同一 tx で複数 attachment を一括 UPDATE するので、
/// `id ASC` が事実上「TUI で選択した順」とほぼ一致する。
/// (TUI は upload → `attachment_ids` 配列で順序を渡すが、現行 schema には
/// 添付順カラムが無いので id 昇順で代用する。)
///
/// permalink AP JSON refetch で `attachment` を出すために導入 (M13 後段)。
pub async fn list_by_note(pool: &PgPool, note_id: i64) -> sqlx::Result<Vec<MediaRow>> {
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        FROM media
        WHERE note_id = $1
        ORDER BY id ASC
        "#,
        note_id,
    )
    .fetch_all(pool)
    .await
}

/// `owner_actor_id` が所有する media を **id 降順** で引く。`MiAuth` の
/// `drive/files` 一覧 (= Aria のドライブ閲覧) 用。`since_id`/`until_id` は
/// Misskey 仕様の **排他** カーソル (`> sinceId` / `< untilId`)。
pub async fn list_by_owner_window(
    pool: &PgPool,
    owner_actor_id: i64,
    since_id: Option<i64>,
    until_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<MediaRow>> {
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        FROM media
        WHERE owner_actor_id = $1
          AND ($2::BIGINT IS NULL OR id > $2)
          AND ($3::BIGINT IS NULL OR id < $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        owner_actor_id,
        since_id,
        until_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `owner_actor_id` が所有する単一 media を id で引く (`MiAuth` `drive/files/show`
/// の所有者ガード込み)。他人の media id を指定しても `None` を返す。
pub async fn get_by_id_for_owner(
    pool: &PgPool,
    id: i64,
    owner_actor_id: i64,
) -> sqlx::Result<Option<MediaRow>> {
    sqlx::query_as!(
        MediaRow,
        r#"
        SELECT id, storage_key, media_type, width, height, byte_size,
               kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        FROM media
        WHERE id = $1 AND owner_actor_id = $2
        "#,
        id,
        owner_actor_id,
    )
    .fetch_optional(pool)
    .await
}

/// `owner_actor_id` が所有する media の `byte_size` 合計 (= ドライブ使用量、bytes)。
/// `MiAuth` `POST /api/drive` (`DriveUsage`) 用。媒体が無ければ 0。お一人様なので
/// 1 ユーザー分の集計しか走らず軽い。
pub async fn total_byte_size_for_owner(pool: &PgPool, owner_actor_id: i64) -> sqlx::Result<i64> {
    let rec = sqlx::query!(
        r#"
        SELECT COALESCE(SUM(byte_size), 0)::BIGINT AS "total!"
        FROM media
        WHERE owner_actor_id = $1
        "#,
        owner_actor_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(rec.total)
}

/// `MiAuth` `drive/files/update` の `comment` (= AP alt text) 更新。所有者ガード
/// 込みで `alt_text` を `new_alt` に上書きし、更新後の行を返す。他人の file や
/// 存在しない id は `None`。`new_alt = None` は `alt_text` を NULL にクリアする。
pub async fn set_alt_text_for_owner(
    pool: &PgPool,
    id: i64,
    owner_actor_id: i64,
    new_alt: Option<&str>,
) -> sqlx::Result<Option<MediaRow>> {
    sqlx::query_as!(
        MediaRow,
        r#"
        UPDATE media
        SET alt_text = $3, updated_at = now()
        WHERE id = $1 AND owner_actor_id = $2
        RETURNING id, storage_key, media_type, width, height, byte_size,
                  kind, alt_text, owner_actor_id, note_id, created_at, updated_at, duration_ms
        "#,
        id,
        owner_actor_id,
        new_alt,
    )
    .fetch_optional(pool)
    .await
}

/// `MiAuth` `drive/files/delete` ── **未添付** (`note_id IS NULL`) の自分の file を
/// 削除する。削除できたら `true`、添付済み / 他人 / 不在は `false` (= DELETE が
/// 0 行)。添付済みを弾くのは、`note.attachments` JSONB スナップショットが
/// `storage_key` を握っている既存 note の画像参照を壊さないため。
///
/// `storage_key` は UNIQUE (= 1 オブジェクト 1 行) なので、呼び出し側はこの行が
/// 消えたら対応する R2 オブジェクトを安全に削除できる (他行と共有しない)。
pub async fn delete_unattached_for_owner(
    pool: &PgPool,
    id: i64,
    owner_actor_id: i64,
) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        r#"
        DELETE FROM media
        WHERE id = $1 AND owner_actor_id = $2 AND note_id IS NULL
        "#,
        id,
        owner_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
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
