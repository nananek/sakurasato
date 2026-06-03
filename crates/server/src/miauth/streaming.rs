//! `GET /streaming` ── Misskey 互換 WebSocket streaming endpoint の **最小 stub**
//! (= #170 / 親 #150)。
//!
//! ## 背景
//!
//! Misskey クライアント (Aria / Milktea / `MissRirica` / Iceshrimp Web) は
//! login 後に `wss://<host>/streaming?i=<token>` を開いてリアルタイム更新を
//! 受け取る。Sakurasato は本 endpoint を実装していなかったため WebSocket
//! upgrade で 404 / 405 を返し、クライアント UI が「接続中…」のまま hang
//! する症状になっていた。
//!
//! ## 実装方針 (= 最小 stub)
//!
//! - WebSocket upgrade を受け、`?i=<token>` クエリで Bearer 認証 (= `MiAuth`
//!   token に `read:account` scope があれば通す)。
//! - クライアントからの message を JSON parse:
//!   - `{ type: "connect", body: { id, channel, params } }` → `{ type:
//!     "connected", body: { id } }` で ack を返す。channel 名は記録だけして
//!     実イベントは送らない。
//!   - `{ type: "disconnect", body: { id } }` → ack 不要、receiver 側で channel
//!     state を捨てる。
//!   - 不明な type → 黙って drop (= 将来追加されても 400 を返さない)。
//! - **イベント送信は一切しない** ── 将来 SSE pubsub と統合する別 issue で
//!   実装。本 stub は接続が切れない (= keep alive) ことだけ保証する。
//! - 30 秒ごとに WebSocket ping を投げる (= Misskey 公式 server 挙動と同じ、
//!   tailscale / cloudflared の idle timeout を回避)。
//!
//! ## AGPL discipline
//!
//! WebSocket message frame の type と shape は [misskey-hub.net](https://misskey-hub.net/docs/api/streaming/)
//! の公開仕様から書き起こした。Misskey 本体 (AGPL-3.0) の TypeScript handler
//! は未参照 ── `[[agpl-discipline-miauth]]` 準拠。

use std::time::Duration;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tokio::time::interval;
use tracing::{debug, warn};

use crate::miauth::auth;
use crate::state::AppState;

const SCOPE_READ_ACCOUNT: &str = "read:account";

/// `?i=<token>` クエリパラメタ。Misskey 公式仕様。
#[derive(Debug, Deserialize)]
pub struct StreamingQuery {
    #[serde(default)]
    pub i: Option<String>,
}

/// `GET /streaming` handler。`WebSocketUpgrade` extractor で WebSocket
/// ハンドシェイクを受ける。
///
/// ## 認証
///
/// `?i=<token>` の Bearer 相当を validate。`read:account` scope を持たない
/// token は upgrade 前に 401 を返す ── これは axum 慣行で
/// [`WebSocketUpgrade`] に `on_upgrade` を呼ぶ前に
/// [`axum::http::StatusCode::UNAUTHORIZED`] レスポンスを返すこと。
pub async fn handle(
    State(state): State<AppState>,
    Query(query): Query<StreamingQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(raw) = query.i.filter(|s| !s.is_empty()) else {
        return auth::unauthorized("missing token (use ?i=<token>)");
    };
    let Some(token_row) = auth::validate_token_raw(&state, &raw).await else {
        return auth::unauthorized("invalid or revoked token");
    };
    if !auth::has_scope(&token_row, SCOPE_READ_ACCOUNT) {
        return auth::forbidden(&format!("missing scope: {SCOPE_READ_ACCOUNT}"));
    }
    auth::mark_used_async(&state, token_row.id);

    // WebSocket upgrade 後の処理 ── 30s ping + JSON message echo (= connect ack)。
    ws.on_upgrade(handle_socket)
}

/// Upgrade 済 WebSocket を受け取り、connect ack + 30s ping だけ実行する。
async fn handle_socket(mut socket: WebSocket) {
    // 30 秒ごとに ping を投げる ── tailscale / cloudflared の idle 切断回避。
    let mut ping_tick = interval(Duration::from_secs(30));
    // 初回 tick は即時発火するので skip (= 接続直後に ping を送らない)。
    ping_tick.tick().await;

    loop {
        tokio::select! {
            biased;
            // 受信。Some(Ok(msg)) = 正常、Some(Err(_)) = 接続エラー、None = close。
            recv = socket.recv() => {
                match recv {
                    Some(Ok(msg)) => {
                        if let Some(reply) = handle_incoming(&msg)
                            && socket.send(reply).await.is_err()
                        {
                            break;
                        }
                        // Close フレームは axum が自動 ack するので break のみ。
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        debug!(?err, "miauth /streaming: recv error, closing socket");
                        break;
                    }
                    None => break,
                }
            }
            // 30s ping。失敗 = 接続切れと判定。
            _ = ping_tick.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// 受信 message を parse して必要なら reply を返す。
///
/// - `Text` → JSON parse → `connect` なら `connected` ack を返す。
/// - `Binary` / `Ping` / `Pong` → reply 不要 (= axum が pong を自動返す)。
/// - 不明な JSON shape → silent drop (= 仕様変更に強い)。
fn handle_incoming(msg: &Message) -> Option<Message> {
    let Message::Text(text) = msg else {
        return None;
    };
    let parsed: JsonValue = serde_json::from_str(text).ok()?;
    let msg_type = parsed.get("type")?.as_str()?;
    match msg_type {
        "connect" => {
            // body.id を ack に echo back。Misskey 公式仕様。
            let id = parsed.get("body").and_then(|b| b.get("id")).cloned();
            let channel = parsed
                .get("body")
                .and_then(|b| b.get("channel"))
                .and_then(|c| c.as_str());
            debug!(?id, ?channel, "miauth /streaming: connect ack");
            let ack = serde_json::json!({
                "type": "connected",
                "body": { "id": id.unwrap_or(JsonValue::Null) },
            });
            let serialized = serde_json::to_string(&ack).unwrap_or_else(|err| {
                warn!(?err, "miauth /streaming: failed to serialize connected ack");
                String::new()
            });
            if serialized.is_empty() {
                None
            } else {
                Some(Message::Text(Utf8Bytes::from(serialized)))
            }
        }
        "disconnect" | "ch" | "s" | "sn" | "un" => {
            // `disconnect` は channel 解除、`ch` 等は channel-specific operation。
            // 本 stub では event 送出していないので何もせず ack 不要。
            None
        }
        other => {
            debug!(msg_type = other, "miauth /streaming: unknown message type");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::Utf8Bytes;

    #[test]
    fn handle_incoming_connect_returns_connected_ack() {
        let raw =
            r#"{"type":"connect","body":{"channel":"homeTimeline","id":"chan-1","params":{}}}"#;
        let msg = Message::Text(Utf8Bytes::from(raw.to_string()));
        let reply = handle_incoming(&msg).expect("connect must produce ack");
        let Message::Text(out) = reply else {
            panic!("expected Text reply");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "connected");
        assert_eq!(v["body"]["id"], "chan-1");
    }

    #[test]
    fn handle_incoming_disconnect_returns_none() {
        let raw = r#"{"type":"disconnect","body":{"id":"chan-1"}}"#;
        let msg = Message::Text(Utf8Bytes::from(raw.to_string()));
        assert!(handle_incoming(&msg).is_none());
    }

    #[test]
    fn handle_incoming_invalid_json_returns_none() {
        let raw = "not json at all";
        let msg = Message::Text(Utf8Bytes::from(raw.to_string()));
        assert!(handle_incoming(&msg).is_none());
    }

    #[test]
    fn handle_incoming_unknown_type_returns_none() {
        let raw = r#"{"type":"future-feature","body":{}}"#;
        let msg = Message::Text(Utf8Bytes::from(raw.to_string()));
        assert!(handle_incoming(&msg).is_none());
    }

    #[test]
    fn handle_incoming_binary_returns_none() {
        let msg = Message::Binary(vec![0u8, 1, 2, 3].into());
        assert!(handle_incoming(&msg).is_none());
    }

    #[test]
    fn handle_incoming_connect_without_id_uses_null() {
        let raw = r#"{"type":"connect","body":{"channel":"homeTimeline"}}"#;
        let msg = Message::Text(Utf8Bytes::from(raw.to_string()));
        let reply = handle_incoming(&msg).expect("connect must produce ack");
        let Message::Text(out) = reply else {
            panic!("expected Text reply");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "connected");
        assert!(v["body"]["id"].is_null());
    }
}
