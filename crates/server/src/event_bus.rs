//! Misskey 互換 `/streaming` (= Aria 等 Misskey-compat client) 向けの **サーバ内
//! イベントバス** (親 Issue #150 / #170)。
//!
//! ## なぜ TUI の [`crate::local_api::stream::TimelineEvent`] と別立てなのか
//!
//! TUI 用 SSE (`/api/v1/stream`) は `TimelineEvent` を serde tagged enum として
//! wire に流し、TUI (`crates/tui`) がそれを 1:1 で mirror して deserialize する。
//! そこに variant を足すと TUI 側の deserialize が壊れる (= unknown variant)。
//! 加えて TUI は現状 `note.created` (ローカル投稿) だけを購読すれば足り、通知・
//! リアクション・boost まで欲しいのは Misskey streaming 側だけ。よって streaming
//! 専用の broadcast を **独立させて** TUI 経路を一切触らない。
//!
//! ## 流れ
//!
//! - publisher: 各 dispatch / local API 経路が DB commit 後に **fire-and-forget**
//!   で [`crate::state::AppState::stream_sender`] に `send` する (購読者ゼロで
//!   `Err` になるのは正常 ── streaming 接続が無いだけ)。
//! - subscriber: [`crate::miauth::streaming`] の WebSocket handler が
//!   `subscribe()` して、購読中の channel / note に対応する frame を組んで push
//!   する。イベントは軽量 (基本は DB の id) で、packing に必要な行は subscriber
//!   側が都度 fetch する ── お一人様サーバなので購読は高々数本、re-fetch のコスト
//!   は無視できる。

use sakurasato_core::model::NotificationRow;

/// streaming 用 broadcast チャンネルの容量。お一人様サーバで購読は数本だが、
/// バースト (連合先からの reaction 連打等) で lag しないよう TUI 用 (64) より
/// 広めに取る。溢れたら subscriber 側が `Lagged` を warn して読み飛ばす。
pub const STREAM_CHANNEL_CAPACITY: usize = 128;

/// リアクションの増減方向。Misskey streaming の `noteUpdated`
/// `reacted` / `unreacted` に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    /// リアクションが 1 件付いた。
    Reacted,
    /// リアクションが 1 件外れた (Undo)。
    Unreacted,
}

/// サーバ内で発生した「streaming に流したい」出来事。
///
/// 各 variant は packing に十分な最小限の id / 値だけを持つ。subscriber が
/// 都度 DB から行を引いて Misskey 形へ pack する。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 新規 note (ローカル投稿 or 受信した followee note)。`homeTimeline` へ。
    Note {
        /// `note` テーブルの id。
        note_id: i64,
    },
    /// followee の boost (`Announce`)。`homeTimeline` に renote frame として。
    Renote {
        /// `announce` テーブルの id。
        announce_id: i64,
    },
    /// in-app 通知が 1 件記録された。`main` channel へ。行そのものを載せる
    /// (id 再 fetch を省く ── insert 直後に手元にあるため)。
    Notification(Box<NotificationRow>),
    /// note のリアクションが増減した。購読中 note の `noteUpdated` へ。
    ReactionUpdated {
        /// 対象 note の id。
        note_id: i64,
        /// リアクション文字列 (`:foo:` / `:foo@host:` / Unicode)。
        reaction: String,
        /// 増減の方向。
        kind: ReactionKind,
    },
}
