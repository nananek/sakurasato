//! Compile-time checked queries against the `announce` table (M11).
//!
//! `Announce` (Boost) を受信したときに、誰がいつどの note を boost したかを
//! 記録する。`(note_id, actor_id)` UNIQUE で二重 boost は idempotent、
//! `ap_id` UNIQUE で `Undo` Announce のターゲット解決に使う。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::model::AnnounceRow;

/// Insert an announce row, or return the existing one for the same
/// `(note_id, actor_id)` pair (idempotent — second receipt of the same
/// `Announce` from a peer's retry must not duplicate).
///
/// `ap_id` of the second receipt is ignored (we keep the first); peers that
/// re-announce with a different activity id are rare and the first-wins
/// rule keeps the Undo target stable.
pub async fn insert_or_get(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    published_at: DateTime<Utc>,
) -> sqlx::Result<AnnounceRow> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        INSERT INTO announce (ap_id, note_id, actor_id, published_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (note_id, actor_id) DO UPDATE
            SET published_at = announce.published_at
        RETURNING id, ap_id, note_id, actor_id, published_at, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        published_at,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<AnnounceRow>> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, published_at, created_at
        FROM announce WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// announce 行を内部 `id` で引く。home timeline の `rn:<announce_id>` カーソル
/// 解決 / `notes/show` の renote 参照に使う。
pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<AnnounceRow>> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, published_at, created_at
        FROM announce WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Delete an announce row by AP id. Returns the number of rows deleted
/// (0 if the announce was never recorded — used by `Undo` for the
/// "we never had it" no-op case).
pub async fn delete_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<u64> {
    sqlx::query!("DELETE FROM announce WHERE ap_id = $1", ap_id)
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
}

/// `(note_id, actor_id)` で 1 行引く ── ローカル user 自身の renote を
/// 取り消すときに使う (= TUI の `B` キー経路は activity id を覚えていなくても
/// 「自分が boost したやつ」を DB 側で解決できる)。`UNIQUE (note_id, actor_id)`
/// 制約があるので row は高々 1 件。
pub async fn get_by_pair(
    pool: &PgPool,
    note_id: i64,
    actor_id: i64,
) -> sqlx::Result<Option<AnnounceRow>> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, published_at, created_at
        FROM announce WHERE note_id = $1 AND actor_id = $2
        "#,
        note_id,
        actor_id,
    )
    .fetch_optional(pool)
    .await
}

/// 複数 Note に対する Announce (= boost) 集計を 1 クエリで取る。
///
/// `reaction::counts_for_notes` と同じ形式で home timeline 描画から呼ばれる。
/// `viewer_actor_id` 視点で「自分も renote したか」を `viewer_renoted` に
/// 載せ返す ── お一人様サーバなので viewer は常に local actor だが、
/// 引数として明示することで Test の安定性を確保する。
pub async fn counts_for_notes(
    pool: &PgPool,
    note_ids: &[i64],
    viewer_actor_id: i64,
) -> sqlx::Result<Vec<AnnounceSummaryRow>> {
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        AnnounceSummaryRow,
        r#"
        SELECT
            note_id as "note_id!",
            COUNT(*) as "count!",
            BOOL_OR(actor_id = $2) as "viewer_renoted!"
        FROM announce
        WHERE note_id = ANY($1)
        GROUP BY note_id
        ORDER BY note_id
        "#,
        note_ids,
        viewer_actor_id,
    )
    .fetch_all(pool)
    .await
}

/// `counts_for_notes` の戻り行。`note_id` ごとに 1 行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnnounceSummaryRow {
    pub note_id: i64,
    pub count: i64,
    pub viewer_renoted: bool,
}

/// `list_home_renote_window` の戻り行 (= home timeline に混ぜる renote 1 件)。
///
/// 元 note (`renoted_note_id`) と renoter (`renoter_actor_id`) は呼び出し側が
/// `note::list_timeline_entries_by_ids` / `actor::list_by_ids` で一括 fetch して
/// 解決する (本クエリは軽量に保つ)。並び・カーソルは `announce_published_at`
/// (= boost した時刻) 基準。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RenoteWindowRow {
    pub announce_id: i64,
    pub announce_ap_id: String,
    pub announce_published_at: DateTime<Utc>,
    pub renoter_actor_id: i64,
    pub renoted_note_id: i64,
}

/// home timeline に混ぜる renote (= 自分 or followee による `Announce`) を、
/// `announce.published_at` 降順・時刻カーソル付きで引く。
///
/// 対象: `actor_id` が viewer 自身 or viewer が `accepted` で follow している
/// actor。元 note の `visibility = direct` は除外 (note timeline と対称)。
/// `since_ts` / `until_ts` は **排他** (`>` / `<`) で、混合タイムラインの時刻
/// カーソルに使う (呼び出し側が `note` 側と同じ境界時刻を渡す)。
///
/// **viewer 可視性は SQL 側で担保** (Issue #253) ── 元 note (= boost 対象) の
/// `followers` 限定可視性を、その author を viewer が follow していなければ
/// 除外する。これにより followee が第三者の followers 限定 note を boost しても
/// viewer に漏れない。`public` / `unlisted` は常に可視、viewer 自身の note も
/// 可視。`direct` は上の `<> 'direct'` で先に除外済み。述語は
/// [`crate::repo::note::list_by_author_window`] と同型で、per-item の follow
/// 引き (旧 `viewer_can_view_entry` ループ) を不要にする。
pub async fn list_home_renote_window(
    pool: &PgPool,
    viewer_actor_id: i64,
    since_ts: Option<DateTime<Utc>>,
    until_ts: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<RenoteWindowRow>> {
    sqlx::query_as!(
        RenoteWindowRow,
        r#"
        SELECT
            ann.id AS announce_id,
            ann.ap_id AS announce_ap_id,
            ann.published_at AS announce_published_at,
            ann.actor_id AS renoter_actor_id,
            ann.note_id AS renoted_note_id
        FROM announce ann
        JOIN note n ON n.id = ann.note_id
        WHERE
            n.visibility <> 'direct'
            AND (
                n.visibility IN ('public', 'unlisted')
                OR n.actor_id = $1
                OR (
                    n.visibility = 'followers'
                    AND EXISTS (
                        SELECT 1 FROM follow
                        WHERE follower_actor_id = $1
                          AND followed_actor_id = n.actor_id
                          AND state = 'accepted'
                    )
                )
            )
            AND (
                ann.actor_id = $1
                OR ann.actor_id IN (
                    SELECT followed_actor_id
                    FROM follow
                    WHERE follower_actor_id = $1 AND state = 'accepted'
                )
            )
            AND ($2::TIMESTAMPTZ IS NULL OR ann.published_at > $2)
            AND ($3::TIMESTAMPTZ IS NULL OR ann.published_at < $3)
        ORDER BY ann.published_at DESC
        LIMIT $4
        "#,
        viewer_actor_id,
        since_ts,
        until_ts,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// **M14 #150 (`MiAuth` `users/notes`)** ── 特定 actor (= `users/notes` の対象
/// ユーザ) が行った renote (`Announce`) を `published_at` 降順・時刻カーソル付き
/// で引く。`list_home_renote_window` の **著者スコープ版**。
///
/// 対象: `ann.actor_id = $1` (= プロフィール所有者本人の boost のみ)。home
/// timeline 版が「自分 or followee」を対象にするのに対し、こちらは単一著者に
/// 限定する。元 note の `visibility = direct` を除外するのは home 版と対称
/// (= boost 可能なのは公開系 note のみという前提)。`since_ts` / `until_ts` は
/// **排他** (`>` / `<`) で、note 側ウィンドウ ([`crate::repo::note::list_by_author_window`])
/// と同じ境界時刻を渡してマージする。
///
/// **viewer 可視性は SQL 側で担保** (Issue #253) ── `viewer_actor_id` を取り、
/// boost 対象 note の `followers` 限定可視性を viewer が author を follow して
/// いなければ除外する。これにより呼び出し側の per-item `viewer_can_view_entry`
/// ループ (renote 件数ぶんの follow 引き = N+1) が不要になる。述語は
/// [`list_home_renote_window`] / [`crate::repo::note::list_by_author_window`]
/// と同型。
pub async fn list_author_renote_window(
    pool: &PgPool,
    author_actor_id: i64,
    viewer_actor_id: i64,
    since_ts: Option<DateTime<Utc>>,
    until_ts: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<RenoteWindowRow>> {
    sqlx::query_as!(
        RenoteWindowRow,
        r#"
        SELECT
            ann.id AS announce_id,
            ann.ap_id AS announce_ap_id,
            ann.published_at AS announce_published_at,
            ann.actor_id AS renoter_actor_id,
            ann.note_id AS renoted_note_id
        FROM announce ann
        JOIN note n ON n.id = ann.note_id
        WHERE
            n.visibility <> 'direct'
            AND ann.actor_id = $1
            AND (
                n.visibility IN ('public', 'unlisted')
                OR n.actor_id = $2
                OR (
                    n.visibility = 'followers'
                    AND EXISTS (
                        SELECT 1 FROM follow
                        WHERE follower_actor_id = $2
                          AND followed_actor_id = n.actor_id
                          AND state = 'accepted'
                    )
                )
            )
            AND ($3::TIMESTAMPTZ IS NULL OR ann.published_at > $3)
            AND ($4::TIMESTAMPTZ IS NULL OR ann.published_at < $4)
        ORDER BY ann.published_at DESC, ann.id DESC
        LIMIT $5
        "#,
        author_actor_id,
        viewer_actor_id,
        since_ts,
        until_ts,
        limit,
    )
    .fetch_all(pool)
    .await
}
