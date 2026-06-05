//! CLI `sakurasato-server prune-remote-notes` の実体 ── 古いリモートノートの GC。
//!
//! 連合タイムラインのリモートノートは放っておくと溜まり続けるが、お一人様サーバ
//! は検索用途を持たないので保持し続ける価値が薄い。本コマンドは `is_local =
//! FALSE` かつ `created_at` が `--older-than` 日より前のノートのうち、**自分が
//! interaction していないもの** を削除する (保存条件は
//! [`repo::note::prune_remote_notes`] 参照)。
//!
//! host の cron / systemd-timer から定期実行する想定。常駐ワーカ化しないのは、
//! Neon の scale-to-zero (= idle で compute サスペンド) を妨げる定期 DB
//! アクセスを増やさないため ── 配送ワーカ以外の常駐ポーラを足さない方針。
//!
//! migrations は走らせない (`init` で済んでいる前提、`Deliver` と同じ)。

use sakurasato_core::repo;

use crate::cli::PruneArgs;
use crate::state::AppState;

/// `prune-remote-notes` サブコマンドの実体。
pub async fn run(config: sakurasato_core::Config, args: PruneArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    // clap は `u32` (最小 1) で受ける。`make_interval(days => ...)` は i32 を取る
    // ので変換する。`u32::MAX` は `i32::MAX` を超え得るが、その帯の値 (≈ 589 万年
    // 以上) は事実上「消さない」なので `i32::MAX` にクランプして panic を避ける
    // (`as i32` の wrap を避けるための `try_from`)。
    let older_than_days = i32::try_from(args.older_than).unwrap_or(i32::MAX);

    let count = repo::note::prune_remote_notes(state.pool(), older_than_days, args.dry_run).await?;

    if args.dry_run {
        println!(
            "[dry-run] {count} remote note(s) older than {} day(s) would be deleted (interaction 済みは除外)",
            args.older_than
        );
    } else {
        println!(
            "deleted {count} remote note(s) older than {} day(s) (interaction 済みは除外)",
            args.older_than
        );
    }
    Ok(())
}
