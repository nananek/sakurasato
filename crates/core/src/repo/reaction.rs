//! Compile-time checked queries against the `reaction` table.

use sqlx::PgPool;

use crate::model::ReactionRow;

pub async fn insert(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    content: &str,
    emoji_id: Option<i64>,
) -> sqlx::Result<ReactionRow> {
    sqlx::query_as!(
        ReactionRow,
        r#"
        INSERT INTO reaction (ap_id, note_id, actor_id, content, emoji_id)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, ap_id, note_id, actor_id, content, emoji_id, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        content,
        emoji_id,
    )
    .fetch_one(pool)
    .await
}

/// `ap_id` で 1 行引く。Inbound `Undo` の対象確認に使う (M8 PR2)。
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<ReactionRow>> {
    sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Idempotent insert (M8 PR2 inbound reaction)。
///
/// 同じ `ap_id` で再送された Activity は同じ行を返す (= リトライ安全)。
/// 別 `ap_id` だが natural key (`note_id`, `actor_id`, `content`) が衝突する
/// 場合 (= 同じ actor が同じ note に同じ content を別 Activity ID で押し付け
/// てきた) も既存行を返す。
///
/// 戻り値は `(行, is_new)`。`is_new` は「本呼び出しで **新規 INSERT した**」
/// かどうか (= 冪等再配送では `false`)。呼び出し側はこれで「既存行への再配送」
/// と「初回受領」を区別し、in-app 通知 / streaming push 等の副作用を初回だけ
/// 発火できる。従来は戻り行の `ap_id` と入力 `ap_id` を比較するヒューリスティック
/// で代用していたが、natural key 衝突 (別 Activity ID) で既存行を返した場合も
/// `false` になる点が正確である。
///
/// 実装: Postgres の `ON CONFLICT (col)` は 1 つの制約しか同時に指定できない
/// ため、`ON CONFLICT DO NOTHING` (= 任意の衝突を抑える) + RETURNING で空が
/// 返ってきたら fallback SELECT で既存行を引く。RETURNING が返ったときのみ
/// `is_new == true` (`fetch_optional` が `None` を返すのは衝突時のみで、その他
/// のエラーは `?` で伝播する)。
pub async fn insert_or_get(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    content: &str,
    emoji_id: Option<i64>,
) -> sqlx::Result<(ReactionRow, bool)> {
    let inserted = sqlx::query_as!(
        ReactionRow,
        r#"
        INSERT INTO reaction (ap_id, note_id, actor_id, content, emoji_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT DO NOTHING
        RETURNING id, ap_id, note_id, actor_id, content, emoji_id, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        content,
        emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    if let Some(row) = inserted {
        // RETURNING が返った = 本呼び出しで新規 INSERT した。
        return Ok((row, true));
    }
    // 衝突 (ap_id か natural key のいずれか) で挿入できなかった。
    // 同じ ap_id を優先して引き、無ければ natural key で引く ──
    // 同じ Activity の再送だった場合に ap_id 一致行を返したいため。
    if let Some(row) = sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok((row, false));
    }
    sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE note_id = $1 AND actor_id = $2 AND content = $3
        "#,
        note_id,
        actor_id,
        content,
    )
    .fetch_one(pool)
    .await
    .map(|row| (row, false))
}

pub async fn delete_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query!("DELETE FROM reaction WHERE ap_id = $1", ap_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// `note_id` に紐付くリアクションを `(content, count)` で集約して返す。
///
/// TUI / Web 表示用 (M8 PR3 で TUI から消費)。content は AP のまま (= `:foo:`
/// 形式や Unicode emoji がそのまま入る)。`emoji_id` の resolution は呼び出し側
/// 責務 ── 連合先によって `emoji_id` の有無が変わるため、表示層で別途引く。
pub async fn count_by_note(pool: &PgPool, note_id: i64) -> sqlx::Result<Vec<ReactionContentCount>> {
    sqlx::query_as!(
        ReactionContentCount,
        r#"
        SELECT
            content as "content!",
            COUNT(*) as "count!",
            MAX(emoji_id) as "any_emoji_id: i64"
        FROM reaction
        WHERE note_id = $1
        GROUP BY content
        ORDER BY MIN(created_at)
        "#,
        note_id,
    )
    .fetch_all(pool)
    .await
}

/// `count_by_note` の結果型。`(content, count, 代表 emoji_id)`。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReactionContentCount {
    pub content: String,
    pub count: i64,
    /// `MAX(emoji_id)` を「代表値」として返す。同じ content (= 同じ shortcode)
    /// に複数の `emoji_id` が紐付くことは想定しないが、片方 NULL / 片方
    /// 既知の絵文字 → 既知側を採用したいので MAX を使う。
    pub any_emoji_id: Option<i64>,
}

/// 複数 Note に対するリアクション集計を 1 クエリで取る (M8 PR3 home timeline)。
///
/// 戻り値の各行は 1 つの (`note_id`, `content`) ペアに対応する。
/// `image_url` / `media_type` / `is_local` は emoji への LEFT JOIN 結果で、
/// `emoji_id` が NULL (= Unicode reaction) の場合は全て NULL。
pub async fn counts_for_notes(
    pool: &PgPool,
    note_ids: &[i64],
) -> sqlx::Result<Vec<ReactionSummaryRow>> {
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        ReactionSummaryRow,
        r#"
        SELECT
            r.note_id as "note_id!",
            r.content as "content!",
            COUNT(*) as "count!",
            MAX(e.id) as "emoji_id: i64",
            MAX(e.image_key) as "image_key: String",
            MAX(e.media_type) as "media_type: String",
            BOOL_OR(e.is_local) as "is_local: bool",
            MIN(r.created_at) as "first_at!"
        FROM reaction r
        LEFT JOIN emoji e ON e.id = r.emoji_id
        WHERE r.note_id = ANY($1)
        GROUP BY r.note_id, r.content
        ORDER BY r.note_id, MIN(r.created_at)
        "#,
        note_ids,
    )
    .fetch_all(pool)
    .await
}

/// `counts_for_notes` の戻り行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReactionSummaryRow {
    pub note_id: i64,
    pub content: String,
    pub count: i64,
    /// 代表 `emoji_id`。同じ shortcode に複数の emoji 行が紐付くことは事実上
    /// 無いが、念のため `MAX` で 1 つ選ぶ。
    pub emoji_id: Option<i64>,
    /// 対応 emoji の `image_key` (= local: `emoji/local/...webp` / remote URL)。
    pub image_key: Option<String>,
    pub media_type: Option<String>,
    pub is_local: Option<bool>,
    /// `MIN(created_at)`。並び替えにだけ使い、API には出さない。
    pub first_at: chrono::DateTime<chrono::Utc>,
}

/// Note 1 件への **個別** reaction 行を `id DESC` で返す (Misskey `notes/reactions`)。
///
/// `counts_for_notes` が `(content)` 単位の **集計** なのに対し、こちらは reactor
/// ごとの 1 行 (= 誰がいつどの絵文字でリアクションしたか) を返す。Misskey の
/// reaction 詳細 (= タップで reactor 一覧) に使う。
///
/// - `type_filter`: `Some` なら content 完全一致で絞る (Misskey の `type` 引数)。
/// - `since_id` / `until_id`: 排他境界 (`id > since` / `id < until`)。
/// - `offset`: `OFFSET`。Misskey は `offset` も受けるので一応対応 (`0` で無効)。
/// - `limit`: `LIMIT`。
pub async fn list_for_note(
    pool: &PgPool,
    note_id: i64,
    type_filter: Option<&str>,
    since_id: Option<i64>,
    until_id: Option<i64>,
    offset: i64,
    limit: i64,
) -> sqlx::Result<Vec<ReactionWithActor>> {
    sqlx::query_as!(
        ReactionWithActor,
        r#"
        SELECT r.id, r.content, r.actor_id, r.created_at
        FROM reaction r
        WHERE r.note_id = $1
          AND ($2::text IS NULL OR r.content = $2)
          AND ($3::bigint IS NULL OR r.id > $3)
          AND ($4::bigint IS NULL OR r.id < $4)
        ORDER BY r.id DESC
        OFFSET $5
        LIMIT $6
        "#,
        note_id,
        type_filter,
        since_id,
        until_id,
        offset,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `list_for_note` の戻り行 (= 個別 reaction + reactor)。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReactionWithActor {
    pub id: i64,
    pub content: String,
    pub actor_id: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// viewer 自身が `note_ids` の各 note に付けた reaction の `content` を 1 query で
/// 引く (= Misskey `Note.myReaction`)。`note_id -> content` の行を返す。
///
/// お一人様サーバなので viewer は常に local actor だが、`announce::counts_for_notes`
/// と対称に `viewer_actor_id` で明示スコープする (correctness + test 安定性)。
///
/// `UNIQUE (note_id, actor_id, content)` 制約上、同じ viewer が 1 note に複数 content
/// で reaction し得るが、Misskey 仕様は note あたり 1 reaction。`DISTINCT ON
/// (note_id)` + `ORDER BY note_id, id` で最古 (= `id` 最小) の 1 件を決定的に選ぶ。
pub async fn my_reactions_for_notes(
    pool: &PgPool,
    note_ids: &[i64],
    viewer_actor_id: i64,
) -> sqlx::Result<Vec<MyReactionRow>> {
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        MyReactionRow,
        r#"
        SELECT DISTINCT ON (note_id)
            note_id as "note_id!",
            content as "content!"
        FROM reaction
        WHERE note_id = ANY($1) AND actor_id = $2
        ORDER BY note_id, id
        "#,
        note_ids,
        viewer_actor_id,
    )
    .fetch_all(pool)
    .await
}

/// `my_reactions_for_notes` の戻り行 (= viewer 自身の reaction content)。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MyReactionRow {
    pub note_id: i64,
    pub content: String,
}
