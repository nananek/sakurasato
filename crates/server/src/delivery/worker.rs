//! 常駐配送ワーカ。
//!
//! `serve::run` から `tokio::spawn` され、`delivery_queue` テーブルから
//! `state IN ('pending', 'failed')` かつ `next_attempt_at <= now()` の行を
//! 一定間隔で拾って [`super::try_deliver_one`] を回す。
//!
//! ## 設計方針
//!
//! - **シングルプロセス前提**: お一人様サーバなので worker は 1 本だけ。
//!   `FOR UPDATE SKIP LOCKED` のような楽観ロックは入れず、`pick_due` の
//!   出力をそのまま順次処理する。M4 で local API が分離 worker を持つ
//!   場合に再評価する。
//! - **graceful shutdown**: `tokio::CancellationToken` を受け取り、`select!`
//!   で polling とキャンセルを競合させる。in-flight な `try_deliver_one`
//!   は完走を待つ ── 中断するとリクエストを撃ち出して結果を記録できず
//!   永遠に `pending` のままになる行が出る (= 二重配送の温床)。
//! - **エラー伝播の制限**: 1 行の失敗は `tracing::warn!` に出して次の行へ。
//!   ループ自体を止めるのは shutdown と pool 異常 (`pick_due` の DB エラー)
//!   のみ。DB エラーは 1 秒待って再試行 (Postgres が瞬間的に詰まることを
//!   想定)。
//! - **ティック間隔**: アイドル時 `IDLE_TICK`、行が有るときは
//!   バッチ完走後にすぐ次のバッチ。次の `next_attempt_at` まで待つ最適化は
//!   M3b-3 のスコープ外 (queue が空になればすぐ idle で sleep するので
//!   無駄ループは起きない)。
//!
//! ## 互換性
//!
//! `delivery::run` (CLI `sakurasato deliver --queue-id N`) と直交。CLI は
//! 1 行手動 flush 用、worker は常時自動 flush 用。

use std::time::Duration;

use sakurasato_core::repo;
use tokio::sync::watch;
use tracing::{info, warn};

use super::try_deliver_one;
use crate::state::AppState;

/// アイドル時のポーリング間隔。queue が空のときはこの間隔で目を覚ます。
const IDLE_TICK: Duration = Duration::from_secs(5);

/// DB エラー時の back-off 秒数。Postgres の瞬間的な詰まりを想定して短く。
const DB_BACKOFF: Duration = Duration::from_secs(1);

/// 1 ティックで拾う最大行数。`pick_due` の LIMIT に渡す。
///
/// 50 は適当値。配送先 inbox は HTTP/2 keep-alive で多重化されるので、
/// 50 行を 1 ティックで投げきっても 1 ホストあたり数本のコネクション
/// で済む。レート制限が必要な相手については相手側 4xx で `mark_failed` に
/// 倒れて backoff にかかるので、ここで人工的に制限する必要はない。
const TICK_BATCH: i64 = 50;

/// ワーカ常駐ループ。
///
/// `shutdown` は `serve::run` 側で SIGINT / SIGTERM が来たら `true` に倒される
/// `watch::Receiver`。`run` はこのレシーバが終端化するか `true` を受け取った
/// 時点でループを抜ける。in-flight な配送 (= `try_deliver_one` の 1 回分) は
/// 中断せず、完走してから次のループ判定に戻る ── 配送結果を DB に書き戻せ
/// なかった場合に同じ activity を二重配送するのを避けるため。
pub async fn run(state: AppState, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    info!(
        idle_tick_secs = IDLE_TICK.as_secs(),
        batch = TICK_BATCH,
        "delivery worker starting",
    );
    loop {
        // 1. shutdown 確認 (バッチ間で都度チェック)。
        if *shutdown.borrow() {
            info!("delivery worker: shutdown requested before tick");
            break;
        }

        // 2. 1 バッチ拾って配送。空ならアイドル sleep へ。
        match repo::delivery_queue::pick_due(state.pool(), TICK_BATCH).await {
            Ok(rows) if rows.is_empty() => {
                tokio::select! {
                    () = tokio::time::sleep(IDLE_TICK) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("delivery worker: shutdown received during idle");
                            break;
                        }
                    }
                }
            }
            Ok(rows) => {
                let batch_size = rows.len();
                for row in rows {
                    if *shutdown.borrow() {
                        info!(
                            queue_id = row.id,
                            "delivery worker: shutdown received mid-batch, stopping",
                        );
                        return Ok(());
                    }
                    if let Err(err) = try_deliver_one(&state, row.id).await {
                        // 個別行の失敗は次の行に進む。`mark_dead` / `mark_failed`
                        // は `try_deliver_one` の中で適切に呼ばれているので、
                        // 同じ行が次の tick で再選択されることはない。
                        warn!(queue_id = row.id, error = %err, "delivery worker: row failed");
                    }
                }
                info!(processed = batch_size, "delivery worker: batch done");
                // バッチ完了直後は次のバッチを即座に試す ── まだ due 行が
                // 残っているかもしれないため。
            }
            Err(err) => {
                warn!(error = %err, "delivery worker: pick_due failed; backing off");
                tokio::select! {
                    () = tokio::time::sleep(DB_BACKOFF) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("delivery worker: shutdown received during DB backoff");
                            break;
                        }
                    }
                }
            }
        }
    }
    info!("delivery worker stopped");
    Ok(())
}

/// `serve::run` から `tokio::spawn` する用のラッパ。エラーログだけ吐いて
/// 上に伝播させない (axum 側のシャットダウンとは別のタスクなので、worker
/// 由来の panic でプロセスを落とす意義は薄い)。
pub fn spawn(state: AppState, shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run(state, shutdown).await {
            warn!(error = %err, "delivery worker exited with error");
        }
    })
}
