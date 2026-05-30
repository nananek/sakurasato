//! `/api/v1/stream` 購読タスク。
//!
//! [`LocalApi::open_stream`] が返した hyper `Response` body をフレーム化して
//! 1 件ずつデコードし、`mpsc::Sender<StreamEvent>` に push する。受信側が
//! drop されると次のループで `tx.is_closed()` が true になり、タスクが
//! 自然に終わる。
//!
//! ## エラー処理
//!
//! - パースできない event は warn を残して continue。
//! - body の読み出し中に IO エラーが出たら 1 秒待って再接続する。

use std::io;
use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use http_body_util::BodyStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::client::{LocalApi, StreamEvent};

/// 1 失敗あたりの再接続待ち。
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// SSE 購読タスクのエントリポイント。
///
/// 受信側 (`tx` の対になる `Receiver`) が drop されると `is_closed()` が
/// true になり、ループを抜ける。
pub async fn run(api: LocalApi, tx: mpsc::Sender<StreamEvent>) {
    loop {
        if tx.is_closed() {
            debug!("SSE task: receiver dropped; exit");
            return;
        }
        match subscribe_once(&api, &tx).await {
            Ok(()) => {
                debug!("SSE stream closed by server; reconnecting");
            }
            Err(err) => {
                warn!(%err, "SSE stream errored; reconnecting in {RECONNECT_DELAY:?}");
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn subscribe_once(
    api: &LocalApi,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<(), SubscribeError> {
    let resp = api.open_stream().await.map_err(SubscribeError::Connect)?;
    let body_stream = BodyStream::new(resp.into_body());

    // hyper Frame → Bytes へ畳む。trailer は捨て、エラーは io::Error に
    // 包んで `Eventsource` まで伝搬する (= SSE 側でエラー終端され reconnect)。
    let bytes_stream = body_stream.filter_map(|res| async move {
        match res {
            Ok(frame) => frame.into_data().ok().map(Ok::<Bytes, io::Error>),
            Err(e) => Some(Err(io::Error::other(format!("hyper body frame: {e}")))),
        }
    });

    // `filter_map` の戻り stream は内部の async closure が `!Unpin` なため、
    // ここで `Box::pin` して Stream::next を呼べる形にする。
    let mut events = Box::pin(bytes_stream.eventsource());

    while let Some(item) = events.next().await {
        match item {
            Err(e) => {
                warn!(?e, "SSE: stream error; will reconnect");
                return Err(SubscribeError::Stream);
            }
            Ok(event) => {
                if event.data.is_empty() {
                    continue;
                }
                match serde_json::from_str::<StreamEvent>(&event.data) {
                    Ok(parsed) => {
                        if tx.send(parsed).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        warn!(?e, raw = %event.data, "SSE: failed to deserialize event");
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
enum SubscribeError {
    Connect(crate::client::ApiError),
    Stream,
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "connect: {e}"),
            Self::Stream => write!(f, "sse stream"),
        }
    }
}

impl std::error::Error for SubscribeError {}
