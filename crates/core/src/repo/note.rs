//! Compile-time checked queries against the `note` table.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::{NoteRow, Visibility};

#[derive(Debug, Clone)]
pub struct NewNote {
    pub ap_id: String,
    pub actor_id: i64,
    pub content: String,
    pub language: Option<String>,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub summary: Option<String>,
    pub visibility: Visibility,
    pub sensitive: bool,
    pub to_recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub attachments: JsonValue,
    pub tags: JsonValue,
    pub is_local: bool,
    pub url: Option<String>,
    /// MFM ソース (migration 0030)。ローカル投稿は生の投稿本文 (= plain text)、
    /// remote note は現状 `None`。AP `Note.source` / `_misskey_content` として
    /// 配送するために DB に保存する。
    pub source: Option<String>,
    pub published_at: DateTime<Utc>,
}

/// Insert a note, returning the stored row.
///
/// **Idempotent on `ap_id`** ── 同じ `ap_id` の行が既に存在するとき、
/// UNIQUE 違反で `Err` を返すのではなく **既存行をそのまま返す**
/// (`ON CONFLICT (ap_id) DO UPDATE SET ap_id = EXCLUDED.ap_id`)。`DO UPDATE`
/// (`DO NOTHING` ではない) を使うのは、並行 INSERT が走っているとき
/// **相手 tx の commit を待ってから** 確定した既存行を `RETURNING` で返すため
/// (`DO NOTHING` + 後追い `SELECT` は未 commit の競合行を取りこぼし得る)。
/// `SET` するのは conflict key (`ap_id`) を自身の値に上書きする no-op だけなので、
/// 既存行の `content` / `summary` 等は**書き換わらない** ── 編集は inbound
/// `Update` (`update_content`) が担う責務で、ここで上書きしない。
///
/// この冪等化により、同一 Note を指す `Announce` / `Create` / fetch が並行
/// して届いても (Issue #270)、一方が UNIQUE 違反 → `DispatchError::Internal`
/// で捨てられる「エラー経路での競合処理」が消え、両方が既存行を得る。
/// 新規 insert 時の `RETURNING` 挙動は従来どおり (挿入した行を返す)。
pub async fn insert<'e, E>(executor: E, new: NewNote) -> sqlx::Result<NoteRow>
where
    E: sqlx::PgExecutor<'e>,
{
    let to_recipients_json =
        serde_json::to_value(&new.to_recipients).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let cc_recipients_json =
        serde_json::to_value(&new.cc_recipients).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        NoteRow,
        r#"
        INSERT INTO note (
            ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients, cc_recipients, attachments, tags, is_local,
            url, source, published_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16, $17
        )
        ON CONFLICT (ap_id) DO UPDATE SET ap_id = EXCLUDED.ap_id
        RETURNING
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, source, published_at, edited_at, created_at, updated_at
        "#,
        new.ap_id,
        new.actor_id,
        new.content,
        new.language,
        new.in_reply_to_ap_id,
        new.in_reply_to_note_id,
        new.summary,
        new.visibility.as_str(),
        new.sensitive,
        to_recipients_json,
        cc_recipients_json,
        new.attachments,
        new.tags,
        new.is_local,
        new.url,
        new.source,
        new.published_at,
    )
    .fetch_one(executor)
    .await
}

/// Update content / summary / `edited_at` on an existing note. Returns the
/// updated row, or `Ok(None)` if `ap_id` matched no row. Used by the
/// inbound `Update`/`Note` dispatcher (M11) — only the editable fields are
/// touched, the rest (visibility, attachments, etc.) stays as the original.
///
/// The caller is responsible for verifying that the editor is the original
/// author (`note.actor_id == signer.id`) before invoking this.
pub async fn update_content<'e, E>(
    executor: E,
    ap_id: &str,
    content: &str,
    summary: Option<&str>,
    edited_at: chrono::DateTime<chrono::Utc>,
) -> sqlx::Result<Option<NoteRow>>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        NoteRow,
        r#"
        UPDATE note
        SET content = $1,
            summary = $2,
            edited_at = $3,
            updated_at = now()
        WHERE ap_id = $4
        RETURNING
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, source, published_at, edited_at, created_at, updated_at
        "#,
        content,
        summary,
        edited_at,
        ap_id,
    )
    .fetch_optional(executor)
    .await
}

/// Delete a note row by AP id. Returns the number of rows deleted (0 if
/// no row matched — used by the inbound `Delete` dispatcher for the
/// "we never had it" no-op case).
pub async fn delete_by_ap_id<'e, E>(executor: E, ap_id: &str) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query!("DELETE FROM note WHERE ap_id = $1", ap_id)
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
}

/// 古いリモートノートを GC する。`is_local = FALSE` かつ `created_at` が
/// `older_than_days` 日より前のノートのうち、**ローカル actor が interaction
/// していないもの** を削除する。返り値は (削除した | dry-run なら削除予定の) 件数。
///
/// 保存 (= 削除しない) 条件 ── 次のいずれかに該当するリモートノートは残す:
/// - ローカル actor が **reaction** した (`reaction`)。
/// - ローカル actor が **announce (boost / renote)** した (`announce`)。
/// - ローカルノートの **返信先** になっている (`note.in_reply_to_note_id`)。
///
/// これらは `note` への FK が `ON DELETE CASCADE` (`reaction` / `announce`) ない
/// し `SET NULL` (`in_reply_to_note_id`) なので、消すと自分の interaction 記録や
/// 会話文脈が失われる。ローカルノート (= 自分の投稿) は `is_local = FALSE` 条件で
/// 当然対象外。
///
/// `dry_run = true` のときは同じ `DELETE` をトランザクション内で実行して件数だけ
/// 数え、**rollback** する ── 件数集計と実削除で WHERE 句が乖離しないようにする
/// ため、SELECT COUNT を別に持たず DELETE 1 本を真実とする。
pub async fn prune_remote_notes(
    pool: &PgPool,
    older_than_days: i32,
    dry_run: bool,
) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let result = sqlx::query!(
        r#"
        DELETE FROM note n
        WHERE n.is_local = FALSE
          AND n.created_at < now() - make_interval(days => $1)
          AND NOT EXISTS (
              SELECT 1 FROM note rep
              WHERE rep.is_local = TRUE AND rep.in_reply_to_note_id = n.id
          )
          AND NOT EXISTS (
              SELECT 1 FROM reaction r
              JOIN actor a ON a.id = r.actor_id
              WHERE r.note_id = n.id AND a.is_local = TRUE
          )
          AND NOT EXISTS (
              SELECT 1 FROM announce an
              JOIN actor a ON a.id = an.actor_id
              WHERE an.note_id = n.id AND a.is_local = TRUE
          )
        "#,
        older_than_days,
    )
    .execute(&mut *tx)
    .await?;
    let count = result.rows_affected();
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(count)
}

/// Insert 後に `ap_id` と `url` を「実 id を埋めた canonical URL」に書き
/// 直すヘルパ。POST /api/v1/notes で `note.id` 採番後にしか canonical URL
/// が決まらない (= `https://<host>/notes/{id}`) ため、insert → update の
/// 2 段で生成する。
///
/// 同一トランザクションで呼ぶ前提なので executor を取る。
pub async fn set_ap_id_and_url<'e, E>(
    executor: E,
    id: i64,
    ap_id: &str,
    url: &str,
) -> sqlx::Result<()>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query!(
        "UPDATE note SET ap_id = $1, url = $2, updated_at = now() WHERE id = $3",
        ap_id,
        url,
        id,
    )
    .execute(executor)
    .await
    .map(|_| ())
}

pub async fn get_by_ap_id<'e, E>(executor: E, ap_id: &str) -> sqlx::Result<Option<NoteRow>>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        NoteRow,
        r#"
        SELECT
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, source, published_at, edited_at, created_at, updated_at
        FROM note WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(executor)
    .await
}

/// Home timeline 用に Note 行 + actor の表示情報を join して返す。
///
/// 対象 = `viewer_actor_id` 本人の投稿、または `viewer_actor_id` が
/// state='accepted' で follow している actor の投稿。`direct` だけは外す
/// (本人宛 DM の閲覧は別 API で扱う予定 ── M? 以降)。
///
/// 件数は `limit`、カーソルは `before_id` (= `note.id` を opaque な
/// `i64` 整数として扱う)。`before_id = None` の場合は最新から `limit` 件。
/// 結果は `id DESC` 順 (= 作成順 / `BIGSERIAL` の自然な単調列に依存)。
/// `published_at` ではなく `id` で並べることで、リモートから到着した
/// 古い投稿を新しい順 (= 我々が受信した順) で表示できる。
#[allow(clippy::similar_names)]
pub async fn list_home_timeline(
    pool: &PgPool,
    viewer_actor_id: i64,
    before_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE
            n.visibility <> 'direct'
            AND (
                n.actor_id = $1
                OR n.actor_id IN (
                    SELECT followed_actor_id
                    FROM follow
                    WHERE follower_actor_id = $1 AND state = 'accepted'
                )
            )
            AND ($2::BIGINT IS NULL OR n.id < $2)
        ORDER BY n.id DESC
        LIMIT $3
        "#,
        viewer_actor_id,
        before_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// **M14 #159** ── Misskey `notes/timeline` 互換のカーソル付き home timeline。
///
/// `since_id` / `until_id` は **両方排他** (`>` / `<`)。日時版 `since_date` /
/// `until_date` も同様に **排他** (`>` / `<`)。これは Misskey の wire 仕様準拠で、
/// クライアントが「最後に見た id 以降の新規」を取るのに `sinceId = last` を使う
/// (= last 自身は重複取得しない)。
///
/// `list_home_timeline` (= 既存) は `before_id` 1 本のみのカーソルだったので、
/// 本関数は **Misskey 互換専用** の別ラッパとして追加した。可視性フィルタは
/// 既存と同じ「自分 OR follow 中 + visibility != direct」。
#[allow(clippy::similar_names)]
pub async fn list_home_timeline_window(
    pool: &PgPool,
    viewer_actor_id: i64,
    since_id: Option<i64>,
    until_id: Option<i64>,
    since_date: Option<DateTime<Utc>>,
    until_date: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE
            n.visibility <> 'direct'
            AND (
                n.actor_id = $1
                OR n.actor_id IN (
                    SELECT followed_actor_id
                    FROM follow
                    WHERE follower_actor_id = $1 AND state = 'accepted'
                )
            )
            AND ($2::BIGINT IS NULL OR n.id > $2)
            AND ($3::BIGINT IS NULL OR n.id < $3)
            AND ($4::TIMESTAMPTZ IS NULL OR n.published_at > $4)
            AND ($5::TIMESTAMPTZ IS NULL OR n.published_at < $5)
        ORDER BY n.id DESC
        LIMIT $6
        "#,
        viewer_actor_id,
        since_id,
        until_id,
        since_date,
        until_date,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `list_home_timeline` の戻り行。Note の通常カラムに加え、actor 表示
/// 情報を join 同行に持つ。
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct TimelineEntry {
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
    pub attachments: Json<JsonValue>,
    pub tags: Json<JsonValue>,
    pub is_local: bool,
    pub url: Option<String>,
    pub published_at: DateTime<Utc>,
    /// M11: `Update`/`Note` 受領で動く編集時刻。初回は `None`。
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    pub actor_display_name: Option<String>,
    /// 投稿主のアバター URL。リモート actor は HTTP(S) URL、ローカル actor は
    /// 自インスタンス上の `/media/...` (M4 で配信開始)。M5 PR2 の TUI 画像表示
    /// で使う ── 画像取得とデコードは server ではなく TUI 側で行う (CLAUDE.md §7)。
    pub actor_icon_url: Option<String>,
}

/// **M14 #159** ── `notes/show` 用: 単一 Note を actor 表示情報と join 同行で取る。
///
/// `list_home_timeline*` と同じ `TimelineEntry` を 1 件だけ返すヘルパ。
/// 可視性フィルタは適用しない (= visibility は呼び出し側で判定する)。
#[allow(clippy::similar_names)]
pub async fn get_timeline_entry_by_id(
    pool: &PgPool,
    id: i64,
) -> sqlx::Result<Option<TimelineEntry>> {
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// `ids` に含まれる note の `TimelineEntry` をまとめて引く。
///
/// home timeline に renote (= `announce`) を混ぜる際、boost された元 note を
/// 一括取得して `MissNote` に変換するために使う。順序は保証しないので、呼び出し
/// 側で `id -> entry` の map を作って参照すること。空配列なら空を返す。
pub async fn list_timeline_entries_by_ids(
    pool: &PgPool,
    ids: &[i64],
) -> sqlx::Result<Vec<TimelineEntry>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.id = ANY($1)
        "#,
        ids,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<NoteRow>> {
    sqlx::query_as!(
        NoteRow,
        r#"
        SELECT
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, source, published_at, edited_at, created_at, updated_at
        FROM note WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// `NodeInfo.usage.localPosts` 用 ── `is_local = true` の Note の総数を返す。
///
/// お一人様サーバなので件数は単純なスカラで十分。直近の `active_users` 推定にも
/// 「`local_posts > 0 ? 1 : 0`」で接続している (= 投稿が 1 件でもあれば actor
/// は active 扱い)。
pub async fn count_local(pool: &PgPool) -> sqlx::Result<i64> {
    // count(*) は常に非 NULL だが sqlx の型推論では Option<i64> になる。
    let count: Option<i64> = sqlx::query_scalar!("SELECT count(*) FROM note WHERE is_local = TRUE")
        .fetch_one(pool)
        .await?;
    Ok(count.unwrap_or(0))
}

/// **M13 PR3 (Issue #79) `GET /api/v1/actor/{id}/notes`** ── 指定 actor が
/// author の Note を `note.id DESC` 順 (= 受信順) で列挙する。
///
/// ## Visibility filter
///
/// `viewer_actor_id` (= ローカル actor) の視点で見える投稿だけを返す:
///
/// - **author 自身** (`viewer_actor_id == author_actor_id`) → 全 visibility
///   (自分の投稿は direct も含めて全部見える)。
/// - **author 以外** → 以下のいずれか:
///   - `visibility = 'public' | 'unlisted'` → 常に見える。
///   - `visibility = 'followers'` → viewer が `state = 'accepted'` で author を
///     follow しているときのみ見える。
///   - `visibility = 'direct'` → viewer の `ap_id` が `to_recipients` または
///     `cc_recipients` に含まれているときのみ見える。
///
/// `viewer_ap_id` は direct 判定の宛先一致用。JSON 配列で
/// `[viewer_ap_id]` を作って `@>` (包含演算子) で問い合わせる ──
/// `to_recipients` / `cc_recipients` は `jsonb` 配列なので index も
/// (将来) GIN で効かせられる。
///
/// `before_id` / `limit` は `list_home_timeline` と同じカーソル方式。
#[allow(clippy::similar_names)]
pub async fn list_by_author(
    pool: &PgPool,
    author_actor_id: i64,
    viewer_actor_id: i64,
    viewer_ap_id: &str,
    before_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    let viewer_inbox_array =
        serde_json::to_value([viewer_ap_id]).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.actor_id = $1
          AND (
            $1 = $2
            OR n.visibility IN ('public', 'unlisted')
            OR (
              n.visibility = 'followers'
              AND EXISTS (
                SELECT 1 FROM follow
                WHERE follower_actor_id = $2
                  AND followed_actor_id = $1
                  AND state = 'accepted'
              )
            )
            OR (
              n.visibility = 'direct'
              AND (n.to_recipients @> $3::jsonb OR n.cc_recipients @> $3::jsonb)
            )
          )
          AND ($4::BIGINT IS NULL OR n.id < $4)
        ORDER BY n.id DESC
        LIMIT $5
        "#,
        author_actor_id,
        viewer_actor_id,
        viewer_inbox_array,
        before_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// **M14 #150 (`MiAuth` `users/notes`)** ── `list_by_author` の Misskey
/// `users/notes` 互換版。著者本人の Note を `viewer` 視点の可視性で絞った上で、
/// `notes/timeline` と同じ **時刻ウィンドウ** (`published_at` 排他境界) +
/// `withReplies` / `withFiles` フィルタを掛けて引く。
///
/// ## `list_by_author` との差分
///
/// - カーソルが `before_id` (= note id 1 本) ではなく `since_date` / `until_date`
///   (= `published_at` の **排他** 境界 `>` / `<`)。これは `users/notes` が
///   renote (= 別連番の `announce`) と時刻順マージされるため、id ではなく
///   時刻でページングする必要があるから (`list_home_timeline_window` と対称)。
/// - `with_replies = false` のとき返信を除外する。ただし **自己スレッド**
///   (= 自分の note への返信) は Misskey 仕様どおり残す ── 親 note が自分の
///   ものか `in_reply_to_note_id` で判定する。親 note を我々が把握していない
///   remote 返信 (`in_reply_to_note_id IS NULL` だが `in_reply_to_ap_id` あり)
///   は「他者宛返信」とみなして除外する。
/// - `with_files = true` のとき添付のある note だけに絞る (= Misskey の
///   「メディア」タブ)。`attachments` は JSONB 配列なので `jsonb_array_length`。
///
/// 並びは `published_at DESC`、同時刻は `id DESC` で決定的に。可視性述語は
/// [`list_by_author`] と完全に同一 (= 自分は全部、他者は public/unlisted は常時・
/// followers は accepted follow 時・direct は audience 一致時)。
///
/// ## カーソルの既知の制約 (= `notes/timeline` と共有)
///
/// 境界は **`published_at` 一本** (排他)。これは note (= `note.id`) と renote
/// (= 別連番の `announce.id`) を時刻順マージするために id ではなく時刻で
/// ページングする必要があるため ([`crate::miauth`] の `users/notes` /
/// `notes/timeline` ハンドラ参照)。`ORDER BY` は `id DESC` を tiebreak に持つので
/// **同一ページ内** の順序は決定的だが、**ページ境界に秒以下まで同一の
/// `published_at` が複数並ぶ** と排他境界が取りこぼし得る (= 2 つの id 空間を
/// またぐ複合カーソルが組めないため)。ローカル note は µs 精度の `now()` で
/// 衝突しにくく、実害は remote の秒精度 timestamp が同秒に密集した稀ケースに
/// 限られる。`list_home_timeline_window` 経由の home timeline と同じ既知の
/// トレードオフで、本関数で新たに悪化させてはいない。
#[allow(clippy::similar_names, clippy::too_many_arguments)]
pub async fn list_by_author_window(
    pool: &PgPool,
    author_actor_id: i64,
    viewer_actor_id: i64,
    viewer_ap_id: &str,
    with_replies: bool,
    with_files: bool,
    since_date: Option<DateTime<Utc>>,
    until_date: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    let viewer_inbox_array =
        serde_json::to_value([viewer_ap_id]).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.actor_id = $1
          AND (
            $1 = $2
            OR n.visibility IN ('public', 'unlisted')
            OR (
              n.visibility = 'followers'
              AND EXISTS (
                SELECT 1 FROM follow
                WHERE follower_actor_id = $2
                  AND followed_actor_id = $1
                  AND state = 'accepted'
              )
            )
            OR (
              n.visibility = 'direct'
              AND (n.to_recipients @> $3::jsonb OR n.cc_recipients @> $3::jsonb)
            )
          )
          AND (
            $4
            OR n.in_reply_to_ap_id IS NULL
            OR EXISTS (
              SELECT 1 FROM note p
              WHERE p.id = n.in_reply_to_note_id AND p.actor_id = n.actor_id
            )
          )
          AND (NOT $5 OR jsonb_array_length(n.attachments) > 0)
          AND ($6::TIMESTAMPTZ IS NULL OR n.published_at > $6)
          AND ($7::TIMESTAMPTZ IS NULL OR n.published_at < $7)
        ORDER BY n.published_at DESC, n.id DESC
        LIMIT $8
        "#,
        author_actor_id,
        viewer_actor_id,
        viewer_inbox_array,
        with_replies,
        with_files,
        since_date,
        until_date,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// **#150 (Aria fix)** ── `notes/mentions` 用: viewer が明示的に宛先
/// (`to`/`cc`) に含まれる他者の note を時刻窓で列挙する。
///
/// mention 判定は [`list_by_author_window`] の direct-visibility 節と同じ
/// `to_recipients @> viewer_ap_id` / `cc_recipients @> viewer_ap_id` 述語を
/// 使う ── inbound `Create` 受領時の「我々宛」判定
/// ([`crate::dispatch` 相当, `addresses_us`]) と同一の定義に揃えることで、
/// 受信して保存した note と一覧に出る note の集合が食い違わない。
///
/// - `n.actor_id <> viewer_actor_id` で自分自身の note (自己 mention) を除く。
/// - `following_only` (Misskey `following` パラメータ) で著者を `accepted`
///   follow しているものだけに絞れる。
/// - `visibility_filter` (Misskey `visibility` パラメータ) は呼び出し側で
///   Misskey 語彙 → 内部語彙に変換済みの文字列を渡す。`None` なら無指定。
/// - visibility による可視性ゲートは行わない ── 定義上 viewer が to/cc に
///   含まれる note は常に viewer に見える。
#[allow(clippy::too_many_arguments)]
pub async fn list_mentions_window(
    pool: &PgPool,
    viewer_actor_id: i64,
    viewer_ap_id: &str,
    following_only: bool,
    visibility_filter: Option<&str>,
    since_date: Option<DateTime<Utc>>,
    until_date: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    let viewer_inbox_array =
        serde_json::to_value([viewer_ap_id]).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.actor_id <> $1
          AND (n.to_recipients @> $2::jsonb OR n.cc_recipients @> $2::jsonb)
          AND (
            NOT $3
            OR EXISTS (
              SELECT 1 FROM follow
              WHERE follower_actor_id = $1
                AND followed_actor_id = n.actor_id
                AND state = 'accepted'
            )
          )
          AND ($4::TEXT IS NULL OR n.visibility = $4)
          AND ($5::TIMESTAMPTZ IS NULL OR n.published_at > $5)
          AND ($6::TIMESTAMPTZ IS NULL OR n.published_at < $6)
        ORDER BY n.published_at DESC, n.id DESC
        LIMIT $7
        "#,
        viewer_actor_id,
        viewer_inbox_array,
        following_only,
        visibility_filter,
        since_date,
        until_date,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// バックフィルCLI (`sakurasato-server emoji backfill-remote`) 向け:
/// 蓄積済みリモート Note のうち `tag` (= AP `tag`、custom emoji を含みうる)
/// が非空のものを id 昇順で keyset page する。
///
/// カーソルは `published_at` (リモート由来で信頼できない値) ではなく
/// `n.id` (DB 採番の単調増加 PK) を使う。`tags` は `NOT NULL DEFAULT '[]'`
/// (migration 0002) だが、これは列が NULL にならないことしか保証しない ──
/// `dispatch/note.rs::build_remote_note` は `obj.get("tag")` の型を検証
/// せずそのまま格納しているため、AS2/JSON-LD の圧縮表現 (要素 1 件のとき
/// 配列でなく単一オブジェクトになる等) により **配列でない値**が入った行が
/// 存在しうる。`jsonb_array_length()` は非配列に対して例外を投げるため、
/// `jsonb_typeof(...) = 'array'` (例外を投げない) で先に型を確認してから
/// `n.tags <> '[]'::jsonb` (空配列比較、こちらも関数呼び出しではないので
/// 例外なし) で非空判定する ── 1 行でも `jsonb_array_length` に到達しない
/// 書き方にすることで、backfill CLI が異常データ 1 件で全体クラッシュ
/// しないようにする。`type == "Emoji"` かどうかの最終判定は呼び出し側
/// (Rust, `learn_note_emoji_tags`) が `.as_array()` で行う。
#[derive(Debug, Clone)]
pub struct RemoteNoteTagsRow {
    pub id: i64,
    pub ap_id: String,
    /// 著者 actor の AP id (host 解決用)。
    pub actor_ap_id: String,
    pub tags: Json<JsonValue>,
}

pub async fn list_remote_note_tags_since_id(
    pool: &PgPool,
    after_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<RemoteNoteTagsRow>> {
    sqlx::query_as!(
        RemoteNoteTagsRow,
        r#"
        SELECT n.id, n.ap_id, a.ap_id AS actor_ap_id,
               n.tags as "tags: Json<JsonValue>"
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE n.is_local = FALSE
          AND jsonb_typeof(n.tags) = 'array'
          AND n.tags <> '[]'::jsonb
          AND ($1::BIGINT IS NULL OR n.id > $1)
        ORDER BY n.id ASC
        LIMIT $2
        "#,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await
}
