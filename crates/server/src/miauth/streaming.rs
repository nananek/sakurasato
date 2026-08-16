//! `GET /streaming` ── Misskey 互換 WebSocket streaming endpoint (#170 / 親 #150)。
//!
//! ## 背景
//!
//! Misskey クライアント (Aria / Milktea / `MissRirica` / Iceshrimp Web) は
//! login 後に `wss://<host>/streaming?i=<token>` を開いてリアルタイム更新を
//! 受け取る。初期は接続を保つだけの stub だったが、本 module でサーバ内イベント
//! バス ([`crate::event_bus`]) を購読して **実イベントを push** する。
//!
//! ## 対応チャンネル / イベント
//!
//! - `homeTimeline` / `hybridTimeline` (= `ChannelKind::Home`)
//!   - 新規 note (ローカル投稿 + 受信 followee note) → `{type:"channel",
//!     body:{id, type:"note", body:<MissNote>}}`
//!   - followee の boost (`Announce`) → 同じく `type:"note"` に renote `MissNote`
//! - `main` (= `ChannelKind::Main`)
//!   - in-app 通知 (mention / reaction / renote / follow / …) →
//!     `{type:"channel", body:{id, type:"notification", body:<Notification>}}`
//! - reaction のライブ増減 → 購読中 note へ top-level
//!   `{type:"noteUpdated", body:{id:<noteId>, type:"reacted"|"unreacted",
//!   body:{reaction}}}`。note の購読は client の `subNote`/`s` か、note frame を
//!   配信した時点での **auto-subscribe** で成立する。
//!
//! `localTimeline` / `globalTimeline` は単一ユーザ server では `homeTimeline` と
//! 実質同義のため connect ack だけ返し、note は push しない (= 将来 alias 追加の
//! 余地を残す)。未知 channel も同様に ack のみ。
//!
//! ## 認証
//!
//! `?i=<token>` の Bearer 相当を validate し、`read:account` scope を要求する
//! (アナーキー `ignore_scope` ON なら scope 検査はスキップ、token 有効性のみ)。
//! upgrade 前に viewer (= お一人様 local actor) を解決して packing に使う。
//!
//! ## keep alive
//!
//! 30 秒ごとに WebSocket ping を投げる (= Misskey 公式 server 挙動、tailscale /
//! cloudflared の idle timeout 回避)。
//!
//! ## AGPL discipline
//!
//! WebSocket message frame の type と shape は [misskey-hub.net](https://misskey-hub.net/docs/api/streaming/)
//! の公開仕様から書き起こした。Misskey 本体 (AGPL-3.0) の TypeScript handler
//! は未参照 ── `[[agpl-discipline-miauth]]` 準拠。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::interval;
use tracing::{debug, warn};

use crate::event_bus::{ReactionKind, StreamEvent};
use crate::miauth::auth;
use crate::miauth::conv::{
    NoteSummary, build_renote_miss_note, bulk_load_note_summaries, from_actor_and_counts,
    resolve_user_emojis, resolve_user_emojis_by_ids, timeline_entry_to_miss_note,
};
use crate::miauth::notes::{resolve_self_actor_id, viewer_can_view_entry};
use crate::miauth::notifications::build_notification;
use crate::state::AppState;

const SCOPE_READ_ACCOUNT: &str = "read:account";

/// `?i=<token>` クエリパラメタ。Misskey 公式仕様。
#[derive(Debug, Deserialize)]
pub struct StreamingQuery {
    #[serde(default)]
    pub i: Option<String>,
}

/// 購読中の channel 種別。Misskey の channel 名を最小限にマップする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelKind {
    /// `homeTimeline` / `hybridTimeline` ── 自分 + followee の note / boost。
    Home,
    /// `main` ── 通知フィード。
    Main,
}

impl ChannelKind {
    /// Misskey の channel 名を [`ChannelKind`] に解決する。未対応 channel は
    /// `None` (= connect ack は返すが event は流さない)。
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "homeTimeline" | "hybridTimeline" => Some(Self::Home),
            "main" => Some(Self::Main),
            _ => None,
        }
    }
}

/// `GET /streaming` handler。`WebSocketUpgrade` extractor で WebSocket
/// ハンドシェイクを受ける。
///
/// `read:account` scope を持たない token は upgrade 前に 401/403 を返す ──
/// axum 慣行で [`WebSocketUpgrade::on_upgrade`] を呼ぶ前にステータスレスポンス
/// を返すこと。
pub async fn handle(
    State(state): State<AppState>,
    Query(query): Query<StreamingQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(raw) = query.i.filter(|s| !s.is_empty()) else {
        return auth::unauthorized("missing token (use ?i=<token>)");
    };
    let token_row = match auth::validate_token_for_scope(&state, &raw, SCOPE_READ_ACCOUNT).await {
        Ok(row) => row,
        Err(e) => return e.into_response(),
    };
    auth::mark_used_async(&state, token_row.id);

    // viewer (= お一人様 local actor) を upgrade 前に解決して packing に使う。
    // 未 init のとき (= None) でも接続は張り、connect ack / ping だけ返す (event
    // は流さない) ── Aria が「接続中…」で hang しないため。
    let viewer = resolve_self_actor_id(&state).await;

    ws.on_upgrade(move |socket| handle_socket(state, viewer, socket))
}

/// 1 接続ぶんの購読状態。
#[derive(Debug, Default)]
struct ConnState {
    /// connect `body.id` → channel 種別。`disconnect` で除去する。
    channels: HashMap<String, ChannelKind>,
    /// `noteUpdated` を配りたい note の id。client の `subNote`/`s` か、note frame
    /// を配信した時点の auto-subscribe で入る。
    notes: HashSet<i64>,
}

impl ConnState {
    /// `Home` channel の connect id を列挙する。
    fn home_channel_ids(&self) -> Vec<String> {
        self.channel_ids(ChannelKind::Home)
    }

    /// `Main` channel の connect id を列挙する。
    fn main_channel_ids(&self) -> Vec<String> {
        self.channel_ids(ChannelKind::Main)
    }

    fn channel_ids(&self, kind: ChannelKind) -> Vec<String> {
        self.channels
            .iter()
            .filter(|(_, k)| **k == kind)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// client からの受信メッセージを状態に反映し、必要なら reply (= connect ack)
    /// を返す。イベント配信は行わない (= [`Self::frames_for_event`] の責務)。
    ///
    /// - `connect` → channel を登録 (未対応 channel でも id は登録しないが ack は
    ///   返す)、`connected` ack を返す。
    /// - `disconnect` → channel を除去。
    /// - `subNote`/`s` → note を購読集合に追加。`unsubNote`/`un` → 除去。
    /// - それ以外 / 不正 JSON → 無視 (= 仕様変更に強い)。
    fn on_client_message(&mut self, msg: &Message) -> Option<Message> {
        let Message::Text(text) = msg else {
            return None;
        };
        let parsed: JsonValue = serde_json::from_str(text).ok()?;
        let msg_type = parsed.get("type")?.as_str()?;
        let body = parsed.get("body");
        match msg_type {
            "connect" => {
                let id = body.and_then(|b| b.get("id")).and_then(JsonValue::as_str);
                let channel = body
                    .and_then(|b| b.get("channel"))
                    .and_then(JsonValue::as_str);
                if let (Some(id), Some(channel)) = (id, channel)
                    && let Some(kind) = ChannelKind::from_name(channel)
                {
                    self.channels.insert(id.to_string(), kind);
                }
                debug!(?id, ?channel, "miauth /streaming: connect");
                // channel が未対応でも Misskey 仕様どおり connected ack は返す。
                let ack = json!({
                    "type": "connected",
                    "body": { "id": id.map_or(JsonValue::Null, |s| json!(s)) },
                });
                Some(text_frame(&ack))
            }
            "disconnect" => {
                if let Some(id) = body.and_then(|b| b.get("id")).and_then(JsonValue::as_str) {
                    self.channels.remove(id);
                }
                None
            }
            "subNote" | "s" | "sr" => {
                if let Some(note_id) = body
                    .and_then(|b| b.get("id"))
                    .and_then(JsonValue::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    self.notes.insert(note_id);
                }
                None
            }
            "unsubNote" | "un" => {
                if let Some(note_id) = body
                    .and_then(|b| b.get("id"))
                    .and_then(JsonValue::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    self.notes.remove(&note_id);
                }
                None
            }
            other => {
                debug!(
                    msg_type = other,
                    "miauth /streaming: unhandled message type"
                );
                None
            }
        }
    }

    /// サーバ内イベントを、この接続の購読状態に応じた WebSocket frame 列に変換
    /// する。DB を都度引いて Misskey 形に pack する。`viewer` が `None` (= actor
    /// 未 init) なら常に空。
    async fn frames_for_event(
        &mut self,
        state: &AppState,
        viewer: Option<i64>,
        event: &StreamEvent,
    ) -> Vec<Message> {
        let Some(viewer) = viewer else {
            return Vec::new();
        };
        match event {
            StreamEvent::Note { note_id } => self.note_frames(state, viewer, *note_id).await,
            StreamEvent::Renote { announce_id } => {
                self.renote_frames(state, viewer, *announce_id).await
            }
            StreamEvent::Notification(row) => self.notification_frames(state, viewer, row).await,
            StreamEvent::ReactionUpdated {
                note_id,
                reaction,
                kind,
            } => self.reaction_frames(*note_id, reaction, *kind),
        }
    }

    /// 新規 note を home channel へ。配信した note は auto-subscribe する。
    async fn note_frames(&mut self, state: &AppState, viewer: i64, note_id: i64) -> Vec<Message> {
        let home_ids = self.home_channel_ids();
        if home_ids.is_empty() {
            return Vec::new();
        }
        let Ok(Some(entry)) = repo::note::get_timeline_entry_by_id(state.pool(), note_id).await
        else {
            return Vec::new();
        };
        // publisher は home スコープの note だけ流すが、direct/followers を
        // 取りこぼさないための保険 (= REST timeline と同じ可視性ゲート)。
        if !viewer_can_view_entry(state, &entry, viewer).await {
            return Vec::new();
        }
        let host = &state.config().server.host;
        let summaries = bulk_load_note_summaries(state.pool(), &[note_id], viewer).await;
        let empty = empty_summary();
        let summary = summaries.get(&note_id).unwrap_or(&empty);
        let user_emojis = resolve_user_emojis_by_ids(state.pool(), host, &[entry.actor_id])
            .await
            .remove(&entry.actor_id)
            .unwrap_or_default();
        let miss = timeline_entry_to_miss_note(&entry, summary, host, &user_emojis);
        let body = serde_json::to_value(&miss).unwrap_or(JsonValue::Null);

        self.notes.insert(note_id); // reaction 増減の noteUpdated を届けるため。
        home_ids
            .iter()
            .map(|id| channel_frame(id, "note", &body))
            .collect()
    }

    /// followee の boost を home channel へ renote frame として。
    // renoter (= boost した actor) / renoted (= 元 note) は AP 用語で対になる。
    #[allow(clippy::similar_names)]
    async fn renote_frames(
        &mut self,
        state: &AppState,
        viewer: i64,
        announce_id: i64,
    ) -> Vec<Message> {
        let home_ids = self.home_channel_ids();
        if home_ids.is_empty() {
            return Vec::new();
        }
        let Ok(Some(announce)) = repo::announce::get_by_id(state.pool(), announce_id).await else {
            return Vec::new();
        };
        let Ok(Some(entry)) =
            repo::note::get_timeline_entry_by_id(state.pool(), announce.note_id).await
        else {
            return Vec::new();
        };
        if !viewer_can_view_entry(state, &entry, viewer).await {
            return Vec::new();
        }
        let Ok(Some(renoter)) = repo::actor::get_by_id(state.pool(), announce.actor_id).await
        else {
            return Vec::new();
        };
        let host = &state.config().server.host;
        let summaries = bulk_load_note_summaries(state.pool(), &[announce.note_id], viewer).await;
        let empty = empty_summary();
        let summary = summaries.get(&announce.note_id).unwrap_or(&empty);
        let user_emojis = resolve_user_emojis_by_ids(state.pool(), host, &[entry.actor_id])
            .await
            .remove(&entry.actor_id)
            .unwrap_or_default();
        let renoted = timeline_entry_to_miss_note(&entry, summary, host, &user_emojis);
        let renoter_emojis = resolve_user_emojis(state.pool(), host, &renoter).await;
        let renoter_user = from_actor_and_counts(&renoter, 0, 0, 0, renoter_emojis);
        let created_at = announce
            .published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let renote = build_renote_miss_note(
            announce.id,
            &announce.ap_id,
            &created_at,
            renoter_user,
            announce.actor_id,
            renoted,
        );
        let body = serde_json::to_value(&renote).unwrap_or(JsonValue::Null);

        self.notes.insert(announce.note_id);
        home_ids
            .iter()
            .map(|id| channel_frame(id, "note", &body))
            .collect()
    }

    /// 通知を main channel へ。
    async fn notification_frames(
        &self,
        state: &AppState,
        viewer: i64,
        row: &sakurasato_core::model::NotificationRow,
    ) -> Vec<Message> {
        let main_ids = self.main_channel_ids();
        if main_ids.is_empty() {
            return Vec::new();
        }
        let host = state.config().server.host.clone();
        // 対象 note があれば reaction/announce 集計を 1 件ぶん load (notifications
        // list と同じ packing に揃える)。
        let summaries = match row.note_id {
            Some(note_id) => bulk_load_note_summaries(state.pool(), &[note_id], viewer).await,
            None => HashMap::new(),
        };
        let mut actor_cache: HashMap<i64, JsonValue> = HashMap::new();
        let notif = build_notification(state, row, &host, &summaries, &mut actor_cache).await;
        main_ids
            .iter()
            .map(|id| channel_frame(id, "notification", &notif))
            .collect()
    }

    /// reaction 増減を購読中 note の `noteUpdated` へ。
    fn reaction_frames(&self, note_id: i64, reaction: &str, kind: ReactionKind) -> Vec<Message> {
        if !self.notes.contains(&note_id) {
            return Vec::new();
        }
        let update_type = match kind {
            ReactionKind::Reacted => "reacted",
            ReactionKind::Unreacted => "unreacted",
        };
        vec![note_updated_frame(
            note_id,
            update_type,
            &json!({ "reaction": reaction }),
        )]
    }
}

/// reaction/announce が無い note 用の空 [`NoteSummary`] fallback。
/// (`notifications::build_notification` 内の同名 fallback と対称)。
fn empty_summary() -> NoteSummary {
    NoteSummary {
        reactions: Vec::new(),
        announce: None,
        my_reaction: None,
    }
}

/// `{type:"channel", body:{id, type:<inner_type>, body:<inner>}}` frame。
fn channel_frame(conn_id: &str, inner_type: &str, inner: &JsonValue) -> Message {
    let frame = json!({
        "type": "channel",
        "body": { "id": conn_id, "type": inner_type, "body": inner.clone() },
    });
    text_frame(&frame)
}

/// `{type:"noteUpdated", body:{id:<noteId>, type:<update_type>, body:<inner>}}` frame。
fn note_updated_frame(note_id: i64, update_type: &str, inner: &JsonValue) -> Message {
    let frame = json!({
        "type": "noteUpdated",
        "body": { "id": note_id.to_string(), "type": update_type, "body": inner.clone() },
    });
    text_frame(&frame)
}

/// JSON を WebSocket Text frame に serialize する。
fn text_frame(value: &JsonValue) -> Message {
    Message::Text(Utf8Bytes::from(value.to_string()))
}

/// Upgrade 済 WebSocket を受け取り、client message 処理 + イベント配信 + 30s ping
/// を回す。
async fn handle_socket(state: AppState, viewer: Option<i64>, mut socket: WebSocket) {
    let mut conn = ConnState::default();
    let mut rx = state.stream_sender().subscribe();

    // 30 秒ごとに ping を投げる ── tailscale / cloudflared の idle 切断回避。
    let mut ping_tick = interval(Duration::from_secs(30));
    // 初回 tick は即時発火するので skip (= 接続直後に ping を送らない)。
    ping_tick.tick().await;

    loop {
        tokio::select! {
            biased;
            // client → server。Some(Ok) = 正常、Some(Err) = 接続エラー、None = close。
            recv = socket.recv() => {
                match recv {
                    Some(Ok(msg)) => {
                        if let Some(reply) = conn.on_client_message(&msg)
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
            // サーバ内イベント → 購読中 channel/note に配信。
            ev = rx.recv() => {
                match ev {
                    Ok(event) => {
                        for frame in conn.frames_for_event(&state, viewer, &event).await {
                            if socket.send(frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(skipped, "miauth /streaming: broadcast lagged; events dropped");
                    }
                    // sender は AppState が抱えるのでプロセス寿命中は閉じない。
                    Err(RecvError::Closed) => break,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> Message {
        Message::Text(Utf8Bytes::from(raw.to_string()))
    }

    #[test]
    fn connect_registers_home_channel_and_acks() {
        let mut conn = ConnState::default();
        let raw = r#"{"type":"connect","body":{"channel":"homeTimeline","id":"h1","params":{}}}"#;
        let reply = conn
            .on_client_message(&text(raw))
            .expect("connect must ack");
        let Message::Text(out) = reply else {
            panic!("expected Text reply");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "connected");
        assert_eq!(v["body"]["id"], "h1");
        assert_eq!(conn.home_channel_ids(), vec!["h1".to_string()]);
        assert!(conn.main_channel_ids().is_empty());
    }

    #[test]
    fn connect_main_channel_registers_as_main() {
        let mut conn = ConnState::default();
        conn.on_client_message(&text(
            r#"{"type":"connect","body":{"channel":"main","id":"m1"}}"#,
        ))
        .expect("ack");
        assert_eq!(conn.main_channel_ids(), vec!["m1".to_string()]);
        assert!(conn.home_channel_ids().is_empty());
    }

    #[test]
    fn connect_unknown_channel_acks_but_registers_nothing() {
        let mut conn = ConnState::default();
        let reply = conn
            .on_client_message(&text(
                r#"{"type":"connect","body":{"channel":"globalTimeline","id":"g1"}}"#,
            ))
            .expect("unknown channel still acks");
        let Message::Text(out) = reply else {
            panic!("expected Text reply");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "connected");
        assert!(conn.channels.is_empty());
    }

    #[test]
    fn disconnect_removes_channel() {
        let mut conn = ConnState::default();
        conn.on_client_message(&text(
            r#"{"type":"connect","body":{"channel":"homeTimeline","id":"h1"}}"#,
        ));
        assert!(
            conn.on_client_message(&text(r#"{"type":"disconnect","body":{"id":"h1"}}"#))
                .is_none()
        );
        assert!(conn.home_channel_ids().is_empty());
    }

    #[test]
    fn sub_and_unsub_note_track_note_ids() {
        let mut conn = ConnState::default();
        conn.on_client_message(&text(r#"{"type":"subNote","body":{"id":"42"}}"#));
        assert!(conn.notes.contains(&42));
        // 短縮 alias `s` も同じ。
        conn.on_client_message(&text(r#"{"type":"s","body":{"id":"7"}}"#));
        assert!(conn.notes.contains(&7));
        conn.on_client_message(&text(r#"{"type":"unsubNote","body":{"id":"42"}}"#));
        assert!(!conn.notes.contains(&42));
        assert!(conn.notes.contains(&7));
    }

    #[test]
    fn invalid_json_and_unknown_type_are_ignored() {
        let mut conn = ConnState::default();
        assert!(conn.on_client_message(&text("not json")).is_none());
        assert!(
            conn.on_client_message(&text(r#"{"type":"future","body":{}}"#))
                .is_none()
        );
        assert!(
            conn.on_client_message(&Message::Binary(vec![0, 1, 2].into()))
                .is_none()
        );
    }

    #[test]
    fn reaction_frames_only_for_subscribed_notes() {
        let mut conn = ConnState::default();
        // 未購読 note には出さない。
        assert!(
            conn.reaction_frames(1, ":blob:", ReactionKind::Reacted)
                .is_empty()
        );
        conn.notes.insert(1);
        let frames = conn.reaction_frames(1, ":blob:", ReactionKind::Reacted);
        assert_eq!(frames.len(), 1);
        let Message::Text(out) = &frames[0] else {
            panic!("expected Text");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "noteUpdated");
        assert_eq!(v["body"]["id"], "1");
        assert_eq!(v["body"]["type"], "reacted");
        assert_eq!(v["body"]["body"]["reaction"], ":blob:");
    }

    #[test]
    fn reaction_frames_unreacted_type() {
        let mut conn = ConnState::default();
        conn.notes.insert(5);
        let frames = conn.reaction_frames(5, "👍", ReactionKind::Unreacted);
        let Message::Text(out) = &frames[0] else {
            panic!("expected Text");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["body"]["type"], "unreacted");
        assert_eq!(v["body"]["body"]["reaction"], "👍");
    }

    #[test]
    fn channel_frame_envelope_shape() {
        let f = channel_frame("c1", "note", &json!({"id": "n1"}));
        let Message::Text(out) = f else {
            panic!("expected Text");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert_eq!(v["type"], "channel");
        assert_eq!(v["body"]["id"], "c1");
        assert_eq!(v["body"]["type"], "note");
        assert_eq!(v["body"]["body"]["id"], "n1");
    }

    #[test]
    fn connect_without_id_acks_with_null() {
        let mut conn = ConnState::default();
        let reply = conn
            .on_client_message(&text(
                r#"{"type":"connect","body":{"channel":"homeTimeline"}}"#,
            ))
            .expect("ack");
        let Message::Text(out) = reply else {
            panic!("expected Text");
        };
        let v: JsonValue = serde_json::from_str(out.as_str()).unwrap();
        assert!(v["body"]["id"].is_null());
        // id が無いと channel は登録できない (auto id 生成はしない)。
        assert!(conn.channels.is_empty());
    }
}
