//! ローカル API クライアント (Unix socket + Bearer + JSON)。
//!
//! `server::local_api` が `/run/sakurasato/local.sock` に提供する
//! `/api/v1/{whoami,timeline/home,notes,stream}` を叩く。本クレートは
//! HTTPS 経由の公開 API は扱わない。
//!
//! # 接続
//!
//! [`hyperlocal::UnixConnector`] + [`hyper_util::client::legacy::Client`] で
//! Unix socket 越しに HTTP/1.1。`hyperlocal::Uri::new(socket, path)` で
//! URI を組み、`Host` ヘッダ相当の authority は "localhost" 固定。
//!
//! # 認証
//!
//! すべてのリクエストに `Authorization: Bearer <token>` を付与する。
//! ソケット側で 0o600 (`server::local_api::bind_socket`) が掛かるので、
//! 同 UID プロセスでないとそもそも繋がらない。
//!
//! # エラー
//!
//! [`ApiError`] が transport / status / 1XX 系を一律に表現する。401/404 は
//! 起動時の whoami で検出して止め、TUI 本体には流さない方針。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::{Deserialize, Serialize};

/// Auth / Host のための固定 authority。Unix socket の場合 hyper は authority
/// を要求するが実際には DNS 解決されないので何でもよい。
const AUTHORITY: &str = "sakurasato.local";

/// 1 リクエスト全体のタイムアウト。TUI 内でハングしないように低めに切る。
/// SSE はこの上限を適用しない (= 別経路で長時間維持)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// レスポンス body の上限。タイムライン 1 ページ最大 80 件 × 6KiB ≒ 480KiB
/// を想定し、攻撃的な誤動作対策で 4 MiB に上限を切る。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// 共有可能なローカル API クライアント。`Clone` 可で各 task が安全に使える。
#[derive(Debug, Clone)]
pub struct LocalApi {
    inner: Client<UnixConnector, Full<Bytes>>,
    socket: PathBuf,
    token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("local api request timed out after {0:?}")]
    Timeout(Duration),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("HTTP {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("invalid JSON in response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid header value: {0}")]
    Header(#[from] http::header::InvalidHeaderValue),
}

impl LocalApi {
    /// 新しいクライアントを構築する。`socket` は実際の Unix socket パス
    /// (例: `/run/sakurasato/local.sock`)、`token` は Bearer トークン。
    ///
    /// 空トークンは構築自体は通すが、最初の認証付きリクエストで `server::token`
    /// の SHA-256 lookup が空文字に対しても比較するため 401 を返す。CLI 側
    /// (`main.rs`) で `resolve_token` がそもそも空文字を弾くので、
    /// ここでは追加検査しない。
    pub fn new(socket: PathBuf, token: String) -> Self {
        Self {
            inner: Client::unix(),
            socket,
            token,
        }
    }

    /// 接続先 socket。診断ログ用。
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// `GET /api/v1/whoami`
    pub async fn whoami(&self) -> Result<Whoami, ApiError> {
        self.get_json("/api/v1/whoami").await
    }

    /// `GET /api/v1/timeline/home`
    pub async fn timeline_home(
        &self,
        before_id: Option<i64>,
        limit: i64,
    ) -> Result<TimelineResponse, ApiError> {
        use std::fmt::Write as _;
        let mut path = format!("/api/v1/timeline/home?limit={limit}");
        if let Some(b) = before_id {
            // i64 は format 上 ASCII 安全。クエリ正書法でいう "key=value"。
            // write! は String への書き込みで失敗しないので unwrap して OK。
            write!(&mut path, "&before_id={b}").expect("write to String");
        }
        self.get_json(&path).await
    }

    /// `POST /api/v1/notes`
    pub async fn create_note(
        &self,
        req: &CreateNoteRequest,
    ) -> Result<CreateNoteResponse, ApiError> {
        let body = serde_json::to_vec(req)?;
        let request = self
            .request_builder(Method::POST, "/api/v1/notes")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/stream` を生 Incoming のまま返す。SSE は呼び出し側
    /// ([`crate::sse`]) で `eventsource-stream` に流す。
    pub async fn open_stream(&self) -> Result<hyper::Response<Incoming>, ApiError> {
        let request = self
            .request_builder(Method::GET, "/api/v1/stream")?
            .header(http::header::ACCEPT, "text/event-stream")
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        // SSE は長時間維持なので REQUEST_TIMEOUT を適用しない。
        let resp = self
            .inner
            .request(request)
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(resp)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let request = self
            .request_builder(Method::GET, path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    fn request_builder(
        &self,
        method: Method,
        path: &str,
    ) -> Result<http::request::Builder, ApiError> {
        let uri: http::Uri = UnixUri::new(&self.socket, path).into();
        let bearer = format!("Bearer {}", self.token);
        Ok(Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::HOST, AUTHORITY)
            .header(AUTHORIZATION, http::HeaderValue::from_str(&bearer)?))
    }

    async fn send(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<Incoming>, ApiError> {
        let fut = self.inner.request(request);
        match tokio::time::timeout(REQUEST_TIMEOUT, fut).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(ApiError::Transport(e.to_string())),
            Err(_) => Err(ApiError::Timeout(REQUEST_TIMEOUT)),
        }
    }
}

async fn decode_json<T: for<'de> Deserialize<'de>>(
    resp: hyper::Response<Incoming>,
) -> Result<T, ApiError> {
    let status = resp.status();
    let bytes = read_body_bytes(resp.into_body()).await?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(ApiError::Status { status, body });
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn read_body_bytes(body: Incoming) -> Result<Bytes, ApiError> {
    use http_body_util::Limited;
    let limited = Limited::new(body, MAX_BODY_BYTES);
    let collected = limited
        .collect()
        .await
        .map_err(|e| ApiError::Transport(format!("read body: {e}")))?;
    Ok(collected.to_bytes())
}

async fn read_body_string(body: Incoming) -> Result<String, ApiError> {
    let bytes = read_body_bytes(body).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── DTO 群: server::local_api の `*Response` と JSON 形を合わせる ────

#[derive(Debug, Clone, Deserialize)]
pub struct Whoami {
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    pub inbox: String,
    #[serde(default)]
    pub outbox: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineResponse {
    pub notes: Vec<TimelineNote>,
    #[serde(default)]
    pub next_before_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineNote {
    pub id: i64,
    pub ap_id: String,
    #[serde(default)]
    pub url: Option<String>,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    #[serde(default)]
    pub actor_display_name: Option<String>,
    /// 投稿主のアバター URL (M5 PR2 で server が timeline 応答に載せる)。
    /// TUI は本フィールドを使って画像 fetch をキックする。
    #[serde(default)]
    pub actor_icon_url: Option<String>,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    #[serde(default)]
    pub in_reply_to_ap_id: Option<String>,
    #[serde(default)]
    pub in_reply_to_note_id: Option<i64>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub is_local: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateNoteRequest {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to_ap_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateNoteResponse {
    pub id: i64,
    pub ap_id: String,
    pub url: String,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub queued_deliveries: usize,
}

/// SSE 経由で配られるタイムラインイベント。`server::local_api::stream::TimelineEvent`
/// に対応する。`kind` フィールドで分岐する内部 tag 形式。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    NoteCreated(Box<NoteCreatedPayload>),
}

/// `note.created` の payload。`TimelineNote` とほぼ同形だが `actor_id` 等の
/// JSON 命名と order は server 側に合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct NoteCreatedPayload {
    pub id: i64,
    pub ap_id: String,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    #[serde(default)]
    pub actor_display_name: Option<String>,
    #[serde(default)]
    pub actor_icon_url: Option<String>,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    #[serde(default)]
    pub url: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

impl NoteCreatedPayload {
    /// SSE から TUI のタイムライン Vec に挿入するための変換。`is_local` と
    /// `actor_id` / `in_reply_to_*` の有無は SSE payload には載らない (= 既知の
    /// 範囲だけ写す)。`is_local` は SSE 側で判別できないので false 既定で
    /// 受け、`whoami.ap_id` と一致したら true に上書きする運用 ([[`crate::app`]])。
    pub fn into_timeline_note(self) -> TimelineNote {
        TimelineNote {
            id: self.id,
            ap_id: self.ap_id,
            url: self.url,
            actor_id: self.actor_id,
            actor_ap_id: self.actor_ap_id,
            actor_preferred_username: self.actor_preferred_username,
            actor_display_name: self.actor_display_name,
            actor_icon_url: self.actor_icon_url,
            content: self.content,
            summary: self.summary,
            language: None,
            visibility: self.visibility,
            sensitive: self.sensitive,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            published_at: self.published_at,
            is_local: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_response_roundtrip() {
        let src = r#"{
            "notes": [
                {
                    "id": 7,
                    "ap_id": "https://x.test/notes/7",
                    "url": "https://x.test/notes/7",
                    "actor_id": 3,
                    "actor_ap_id": "https://x.test/users/me",
                    "actor_preferred_username": "me",
                    "actor_display_name": "Me",
                    "content": "hello",
                    "summary": null,
                    "language": "en",
                    "visibility": "public",
                    "sensitive": false,
                    "in_reply_to_ap_id": null,
                    "in_reply_to_note_id": null,
                    "published_at": "2026-05-30T12:34:56Z",
                    "is_local": true
                }
            ],
            "next_before_id": 7
        }"#;
        let parsed: TimelineResponse = serde_json::from_str(src).unwrap();
        assert_eq!(parsed.notes.len(), 1);
        assert_eq!(parsed.next_before_id, Some(7));
        assert_eq!(parsed.notes[0].content, "hello");
    }

    #[test]
    fn stream_event_note_created_deserializes() {
        let src = r#"{
            "kind": "note_created",
            "id": 11,
            "ap_id": "https://x.test/notes/11",
            "actor_id": 1,
            "actor_ap_id": "https://x.test/users/me",
            "actor_preferred_username": "me",
            "actor_display_name": null,
            "content": "stream test",
            "summary": null,
            "visibility": "public",
            "sensitive": false,
            "url": "https://x.test/notes/11",
            "published_at": "2026-05-30T12:34:56Z"
        }"#;
        let evt: StreamEvent = serde_json::from_str(src).unwrap();
        let StreamEvent::NoteCreated(payload) = evt;
        assert_eq!(payload.id, 11);
        let tn = payload.into_timeline_note();
        assert_eq!(tn.id, 11);
        assert!(!tn.is_local);
    }

    #[test]
    fn create_note_request_skips_none() {
        let req = CreateNoteRequest {
            content: "hi".into(),
            summary: None,
            visibility: Some("public".into()),
            sensitive: None,
            language: None,
            in_reply_to_ap_id: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        // 設定したフィールドだけ JSON に乗る。
        assert!(obj.contains_key("content"));
        assert!(obj.contains_key("visibility"));
        assert!(!obj.contains_key("summary"));
        assert!(!obj.contains_key("sensitive"));
        assert!(!obj.contains_key("language"));
        assert!(!obj.contains_key("in_reply_to_ap_id"));
    }
}
