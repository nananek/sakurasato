//! CLI `sakurasato-server emoji backfill-remote` の実体。
//!
//! リモートの custom emoji は、これまで **リアクション受信時**にのみ
//! `emoji` テーブルへ学習・キャッシュされていた (`dispatch/reaction.rs`)。
//! Note 本文中に使われているだけの絵文字は学習されないため、蓄積済みの
//! 過去 Note には未学習のリモート絵文字が残っている可能性がある。
//! 本コマンドは `is_local = FALSE` のリモート Note を id 昇順で走査し、
//! `tag: [Emoji]` から [`crate::emoji_learn::learn_note_emoji_tags`] で
//! 学習する (受信側のフックは `dispatch/note.rs` / `dispatch/update.rs` に
//! 追加済みなので、本コマンドは「今後受信する分」ではなく「既存分」だけを
//! 対象にする一度きりのバックフィル)。
//!
//! **冪等**: `repo::emoji::upsert_remote` は `ap_id` に ON CONFLICT するため
//! 複数回実行しても安全。`should_skip_fetch` のcache-hit判定により、既学習
//! 分の再実行はDB更新すら行わず高速にスキップされる。内部カーソルは永続化
//! しない ── 中断したら最初からやり直せばよい (既学習分は高速スキップ)。

use sakurasato_core::{Config, repo};

use crate::emoji_learn;
use crate::state::AppState;

/// 1 ページあたりの Note 走査件数。
const PAGE_SIZE: i64 = 500;

/// バックフィル結果の集計。CLI 出力 / テスト assertion 用
/// (`emoji_import.rs::ImportSummary` と同じ位置づけ)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillSummary {
    /// 走査したリモート Note の件数。
    pub scanned: usize,
    /// 見つかった Emoji tag の件数。
    pub emoji_tags_seen: usize,
    /// 学習 (upsert) に成功した件数。
    pub emoji_learned: usize,
}

/// `emoji backfill-remote` サブコマンドの実体。
pub async fn run(config: Config) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let summary = run_with_state(&state).await?;
    println!(
        "scanned {} remote note(s); found {} Emoji tag(s); learned {} remote emoji",
        summary.scanned, summary.emoji_tags_seen, summary.emoji_learned,
    );
    Ok(())
}

/// `run` の本体。`AppState` を直接受け取るため、統合テストで
/// `AppState::from_pool` (`sqlx::test` の一時 DB) を使って検証できる。
pub async fn run_with_state(state: &AppState) -> anyhow::Result<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    let mut after_id: Option<i64> = None;

    loop {
        let rows =
            repo::note::list_remote_note_tags_since_id(state.pool(), after_id, PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            summary.scanned += 1;
            let learned =
                emoji_learn::learn_note_emoji_tags(state, &row.actor_ap_id, &row.tags.0).await;
            summary.emoji_tags_seen += learned.emoji_tags_seen;
            summary.emoji_learned += learned.emoji_learned;
        }
        after_id = rows.last().map(|r| r.id);
    }

    Ok(summary)
}
