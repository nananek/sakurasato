//! `GET /api/v1/stream` ── タイムライン更新を SSE で配る。
//!
//! TUI が接続するときに 1 本 SSE を開き、POST notes / 受信 Note dispatch
//! 経由で `TimelineEvent` が publish されるたびに axum SSE Event に
//! 変換して流す。
//!
//! ## 設計
//!
//! - publisher は [`AppState::timeline_sender`] に `send(event)` するだけ。
//!   サブスクライバ 0 で `Err` が返るがそれは正常 (誰も TUI を開いていない)。
//! - subscriber は接続毎に [`broadcast::Sender::subscribe`] で `Receiver`
//!   を取り、`Receiver::recv` の結果を SSE Event にマップする。
//! - `BroadcastStreamRecvError::Lagged` (過剰イベントで古い順に drop された)
//!   は warn を残してそのまま継続。SSE クライアントは切断/再接続で
//!   `GET /api/v1/timeline/home` を取り直す前提なので、ここで stream を
//!   閉じる必要は無い。
//! - publisher 側 (= `AppState` 自体) が破棄されたケースは `BroadcastStream`
//!   側で stream 終端 (= `None`) に変換され、SSE 接続は graceful に閉じる。
//!
//! ## Keep-alive
//!
//! 15 秒ごとに SSE keep-alive (`:keepalive\n\n`) を送る。Mastodon / Misskey
//! の慣習 (15-30 秒) に近く、reverse proxy (Caddy / nginx) のアイドル
//! タイムアウト既定 60 秒より十分短い。

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::Serialize;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::warn;

use crate::state::AppState;

/// broadcast channel の容量。POST notes バーストに対して 64 件のバッファが
/// あれば常識的な TUI 1〜2 接続では溢れない。溢れた場合は古い順に drop
/// され、subscriber は `Lagged(skipped)` を 1 回受け取って継続する。
pub const TIMELINE_CHANNEL_CAPACITY: usize = 64;

/// SSE で TUI に流すイベント。
///
/// クライアント (TUI) は `event:` フィールドで種別を判別する。M4 PR2 では
/// `note.created` のみ。将来 `reaction.created` / `note.deleted` 等を
/// 追加する想定。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEvent {
    /// 新規 Note が作成された (ローカル投稿 or 受信)。
    NoteCreated(Box<NoteCreatedPayload>),
}

/// `note.created` の payload。タイムライン API の Note サマリと同じ形を
/// 流す ── TUI は SSE で来た Note をそのままタイムラインの先頭に挿入できる。
#[derive(Debug, Clone, Serialize)]
pub struct NoteCreatedPayload {
    pub id: i64,
    pub ap_id: String,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    pub actor_display_name: Option<String>,
    pub content: String,
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub url: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

impl TimelineEvent {
    /// SSE Event の `event:` フィールド名。
    fn event_name(&self) -> &'static str {
        match self {
            Self::NoteCreated(_) => "note.created",
        }
    }
}

pub async fn handle(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.timeline_sender().subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(event) => {
            // serde_json::to_string は `TimelineEvent` (= Box<Payload>) で
            // 失敗する経路が無い (全部 Serialize 可な std 型)。万一失敗
            // しても 1 event だけ skip して継続。
            match serde_json::to_string(&event) {
                Ok(json) => Some(Ok(Event::default().event(event.event_name()).data(json))),
                Err(err) => {
                    warn!(?err, "SSE: failed to serialize TimelineEvent");
                    None
                }
            }
        }
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            // Subscriber が遅すぎて buffer 溢れ → 古い順に skipped 件 drop。
            // TUI 側は切断/再接続で timeline を取り直す前提なので、ここでは
            // warn だけ残して継続する。stream を閉じない (== 接続を切らない)。
            warn!(skipped, "SSE: broadcast receiver lagged; events dropped");
            None
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
