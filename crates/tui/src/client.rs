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

    /// `POST /api/v1/media?kind=...[&alt=...]` ── M7 アップロード経路。
    ///
    /// raw バイト列を `application/octet-stream` で送り、server 側で
    /// media-proxy サニタイズ → versitygw 格納 → DB 登録までを行う。
    /// 戻り値は `media.id` 等を含む JSON。
    ///
    /// **`kind`**: `"avatar"` / `"header"` / `"attachment"` のいずれか。
    /// **`alt`**: 添付時の代替テキスト (a11y)。`None` で省略可。
    pub async fn upload_media(
        &self,
        kind: &str,
        alt: Option<&str>,
        body: Vec<u8>,
    ) -> Result<MediaResponse, ApiError> {
        // クエリ文字列は **必ず await 前に確定** させる ── `Serializer` は
        // 内部に `Cow<'_, [u8]>` を持つため `Send` ではない。スコープを
        // 限定して `String` だけを残すよう block で囲む。
        let path = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("kind", kind);
            if let Some(a) = alt
                && !a.is_empty()
            {
                query.append_pair("alt", a);
            }
            format!("/api/v1/media?{}", query.finish())
        };
        let request = self
            .request_builder(Method::POST, &path)?
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `PATCH /api/v1/actor/profile` ── M7 プロフィール更新。
    ///
    /// `display_name` / `summary` / `icon_media_id` / `image_media_id` を
    /// 個別に設定するか、`clear_*` フラグで明示クリアする。サーバ側で
    /// Update Activity が followers に送出される。
    pub async fn patch_profile(&self, req: &ProfileUpdate) -> Result<ProfileResponse, ApiError> {
        let body = serde_json::to_vec(req)?;
        let request = self
            .request_builder(Method::PATCH, "/api/v1/actor/profile")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `GET /api/v1/media/proxy?url=...&variant=...` ── server 経由 (=
    /// media-proxy 経由) でアバター等の画像を取得する (M6 / Issue #36)。
    ///
    /// 戻り値の `Bytes` は media-proxy が再エンコードした WebP。TUI は受信
    /// バイト列を信頼してデコード/プロトコル変換するだけで OK ──
    /// `image` crate の脆弱性が万一あっても、(1) media-proxy 側で 1 度デコード
    /// 済み、(2) WebP の単純な形に再エンコード済み、(3) サイズ上限と画素数
    /// 上限を強制済み、という多層防御が掛かっている。
    pub async fn fetch_proxy_image(&self, url: &str, variant: &str) -> Result<Bytes, ApiError> {
        // `url` の値には `?` / `&` / `=` / `%` 等が含まれうるので
        // form_urlencoded で必ず percent-encode する ── 生のまま format!
        // すると同パラメータが分解されたり 400 を貰ったりする。
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("url", url)
            .append_pair("variant", variant)
            .finish();
        let path = format!("/api/v1/media/proxy?{query}");
        let request = self
            .request_builder(Method::GET, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        let bytes = read_body_bytes(resp.into_body()).await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(ApiError::Status { status, body });
        }
        Ok(bytes)
    }

    /// `POST /api/v1/reactions` ── ローカル user が自分の Note にリアクション
    /// を付ける (M8 PR3)。`content` は `:foo:` 形式 (ローカル emoji) または
    /// Unicode emoji。失敗時は `ApiError::Status` (400/404 など) を伝播する。
    pub async fn create_reaction(
        &self,
        note_id: i64,
        content: &str,
    ) -> Result<ReactionResponse, ApiError> {
        let body = serde_json::to_vec(&CreateReactionRequest {
            note_id,
            content: content.to_string(),
        })?;
        let request = self
            .request_builder(Method::POST, "/api/v1/reactions")?
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        decode_json(resp).await
    }

    /// `DELETE /api/v1/reactions/{id}` ── 自分のリアクションを取り消す (M8 PR3)。
    pub async fn delete_reaction(&self, reaction_id: i64) -> Result<(), ApiError> {
        let path = format!("/api/v1/reactions/{reaction_id}");
        let request = self
            .request_builder(Method::DELETE, &path)?
            .body(Full::default())
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = read_body_string(resp.into_body()).await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        Ok(())
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
    /// M8 PR3: 受領したリアクション集計 (`content` 単位)。古い server (M7 以前)
    /// と通信した場合は `default` で空 Vec になる。
    #[serde(default)]
    pub reactions: Vec<ReactionSummary>,
}

/// `TimelineNote.reactions` の 1 要素。`server::local_api::timeline::ReactionSummaryDto`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionSummary {
    pub content: String,
    pub count: i64,
    #[serde(default)]
    pub emoji_image_url: Option<String>,
    #[serde(default)]
    pub emoji_media_type: Option<String>,
    /// `Some(true)` = local emoji、`Some(false)` = remote、`None` = Unicode。
    #[serde(default)]
    pub emoji_is_local: Option<bool>,
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
    /// M7: 添付メディア `media.id` の配列。空配列は省略する。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<i64>,
}

/// `POST /api/v1/media` のレスポンス。`server::local_api::media::MediaResponse`
/// と JSON 形を合わせる。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaResponse {
    pub id: i64,
    pub storage_key: String,
    pub url: String,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub byte_size: i64,
    pub kind: String,
    #[serde(default)]
    pub alt_text: Option<String>,
}

/// `PATCH /api/v1/actor/profile` のリクエスト。`server::local_api::profile::ProfileUpdate`
/// と対称形。`clear_*` フラグは「明示的に NULL を書く」指示。
#[derive(Debug, Clone, Default, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "4 clear_* フラグは optional field の null 指示 ── server 側と同形を保つ"
)]
pub struct ProfileUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_display_name: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_media_id: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_icon: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_media_id: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_image: bool,
}

/// `PATCH /api/v1/actor/profile` のレスポンス。
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileResponse {
    pub ap_id: String,
    pub preferred_username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    pub queued_deliveries: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateReactionRequest {
    pub note_id: i64,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionResponse {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub content: String,
    #[serde(default)]
    pub emoji_id: Option<i64>,
    pub queued_deliveries: usize,
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
            // SSE は reactions を運ばない (= 新規 Note は初期状態リアクション 0)。
            // 既存 Note へのリアクション増減は M9 で SSE 拡張する想定。
            reactions: Vec::new(),
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
            attachment_ids: Vec::new(),
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        // 設定したフィールドだけ JSON に乗る。空 Vec の attachment_ids も省略。
        assert!(obj.contains_key("content"));
        assert!(obj.contains_key("visibility"));
        assert!(!obj.contains_key("summary"));
        assert!(!obj.contains_key("sensitive"));
        assert!(!obj.contains_key("language"));
        assert!(!obj.contains_key("in_reply_to_ap_id"));
        assert!(!obj.contains_key("attachment_ids"));
    }

    #[test]
    fn create_note_request_includes_attachment_ids() {
        let req = CreateNoteRequest {
            content: "hi".into(),
            summary: None,
            visibility: None,
            sensitive: None,
            language: None,
            in_reply_to_ap_id: None,
            attachment_ids: vec![1, 2],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["attachment_ids"], serde_json::json!([1, 2]));
    }

    #[test]
    fn profile_update_serializes_only_set_fields() {
        let req = ProfileUpdate {
            display_name: Some("ありす".into()),
            ..ProfileUpdate::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(
            obj.get("display_name").and_then(|v| v.as_str()),
            Some("ありす")
        );
        assert!(!obj.contains_key("clear_display_name"));
        assert!(!obj.contains_key("summary"));
        assert!(!obj.contains_key("icon_media_id"));
    }

    #[test]
    fn profile_update_emits_clear_flags() {
        let req = ProfileUpdate {
            clear_icon: true,
            ..ProfileUpdate::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["clear_icon"], serde_json::json!(true));
        assert!(json.as_object().unwrap().get("icon_media_id").is_none());
    }
}
