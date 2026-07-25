//! media-proxy への Unix socket クライアント (M6)。
//!
//! 本 server は `image` crate を引かない契約 (CLAUDE.md §7) なので、外部
//! 画像の取得とデコードは **すべて** このモジュール経由で隣のコンテナに
//! 投げる。レスポンスは media-proxy が再エンコード済みのバイト列 (WebP)
//! なので、本体は中身を解釈せずそのまま下流 (TUI / ブラウザ) に流すだけで
//! よい。
//!
//! # 接続
//!
//! - hyperlocal + hyper-util ── TUI が server を叩くのと同じスタック。
//! - authority は `media-proxy.local` 固定 (hyper は authority を要求するが
//!   UDS 上では DNS 解決されない)。
//!
//! # エラー
//!
//! [`MediaProxyError`] が transport / status / decode を表現する。呼び出し側
//! (= local API ハンドラ) はこれを HTTP ステータスにマップする。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

/// 1 リクエスト全体の timeout。media-proxy 側は 15s で external GET を切る
/// (= `media_proxy::http_client::REQUEST_TIMEOUT`)、ここはその上に裕度を足した値。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 動画サニタイズ用の timeout。再エンコードしないとはいえ、200MB 級ペイロード
/// の UDS 転送 + box walk には画像より裕度が要る。
const VIDEO_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// レスポンス本文の上限 (= 4 MiB)。media-proxy 経由のアバター/プレビューは
/// 通常 256 KiB 以下、最大でも `max_pixels` から逆算して数 MiB に収まる。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// 動画サニタイズレスポンスの上限。動画は再エンコードしない (≒ 入力と
/// 同サイズで返る) ため、画像用の `MAX_BODY_BYTES` とは別枠で大きく取る。
/// `media_proxy.video.max_bytes` の既定 (200 MiB) に余裕を足した値。
const MAX_VIDEO_BODY_BYTES: usize = 256 * 1024 * 1024;

/// 認証境界外の表示用 authority。実 DNS 解決は行わない。
const AUTHORITY: &str = "media-proxy.local";

/// media-proxy を呼び出すクライアント。`Clone` 安価で `AppState` に乗せる。
#[derive(Debug, Clone)]
pub struct MediaProxyClient {
    inner: Client<UnixConnector, Full<Bytes>>,
    socket: PathBuf,
}

#[derive(Debug, Error)]
pub enum MediaProxyError {
    #[error("media-proxy request timed out after {0:?}")]
    Timeout(Duration),
    #[error("media-proxy transport error: {0}")]
    Transport(String),
    /// media-proxy 側から構造化エラー (`{error, reason}`) が返ってきたケース。
    /// 呼び出し側は `status` / `reason` を見て自分の応答ステータスにマップする。
    #[error("media-proxy returned HTTP {status}: {reason} ({message})")]
    Upstream {
        status: StatusCode,
        reason: String,
        message: String,
    },
    #[error("media-proxy response missing Content-Type")]
    MissingContentType,
    #[error("media-proxy response body too large")]
    TooLarge,
}

/// fetch リクエストの JSON 本文 (media-proxy `POST /v1/image/fetch`)。
#[derive(Debug, Serialize)]
struct FetchBody<'a> {
    url: &'a str,
    variant: &'a str,
}

/// `POST /v1/webfinger/resolve` のリクエスト本文。
#[derive(Debug, Serialize)]
struct ResolveBody<'a> {
    acct: &'a str,
}

/// `POST /v1/webfinger/resolve` の正常レスポンス。
#[derive(Debug, Deserialize)]
pub struct ResolvedActor {
    /// 正規化された `acct:user@host`。
    pub subject: String,
    /// `ActivityPub` actor の URI (`rel=self` + AS2 type の `href`)。
    pub actor_uri: String,
    /// `WebFinger` レスポンスの `aliases[]` をそのまま転載。
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// fetch / sanitize の正常レスポンス。
#[derive(Debug)]
pub struct ProcessedImage {
    /// 変換後のバイト列。常に WebP。
    pub bytes: Bytes,
    /// `Content-Type` ヘッダ。常に `image/webp` だが、media-proxy 側の挙動
    /// 変更に追従できるよう型に持つ。
    pub content_type: String,
    /// 変換後の幅 (px)。media-proxy が `X-Output-Width` で返した値。
    /// パースできなかったときは `None`。M7 のアップロード経路ではメタデータ
    /// として DB に保存する。
    pub width: Option<u32>,
    /// 変換後の高さ (px)。`width` と同じく `X-Output-Height` 由来。
    pub height: Option<u32>,
}

/// 動画 sanitize の正常レスポンス。画像と違い再エンコードしないため
/// `content_type` は入力コンテナ (`video/mp4` | `video/webm`) をそのまま
/// 反映する。`duration_ms` はコンテナヘッダから読んだ再生時間。
#[derive(Debug)]
pub struct ProcessedVideo {
    pub bytes: Bytes,
    pub content_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
}

impl MediaProxyClient {
    /// 既存 socket パスでクライアントを構築する。
    /// socket が無くてもエラーにしない (= 起動時には media-proxy が未起動
    /// のことがある) ── 初回 fetch でエラーになる。
    pub fn new(socket: PathBuf) -> Self {
        Self {
            inner: Client::unix(),
            socket,
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// `POST /v1/image/fetch` — リモート URL を media-proxy 経由で取得し、
    /// 再エンコード済みのバイト列を返す。
    pub async fn fetch_image(
        &self,
        url: &str,
        variant: &str,
    ) -> Result<ProcessedImage, MediaProxyError> {
        let body = serde_json::to_vec(&FetchBody { url, variant })
            .map_err(|e| MediaProxyError::Transport(format!("serialize body: {e}")))?;
        let uri: http::Uri = UnixUri::new(&self.socket, "/v1/image/fetch").into();
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::HOST, AUTHORITY)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| MediaProxyError::Transport(e.to_string()))?;

        let resp = self.send(request, REQUEST_TIMEOUT).await?;
        self.read_processed(resp).await
    }

    /// `POST /v1/webfinger/resolve` — `acct:user@host` を解決して
    /// `ActivityPub` actor URI を返す (M10)。
    ///
    /// `follow <acct>` CLI から呼ぶ。WebFinger 取得は JSON 通信 (= 画像
    /// デコードを伴わない) だが、外向き接続を media-proxy に集約して server
    /// コンテナの egress を絞るため経路を寄せる。
    pub async fn resolve_webfinger(&self, acct: &str) -> Result<ResolvedActor, MediaProxyError> {
        let body = serde_json::to_vec(&ResolveBody { acct })
            .map_err(|e| MediaProxyError::Transport(format!("serialize body: {e}")))?;
        let uri: http::Uri = UnixUri::new(&self.socket, "/v1/webfinger/resolve").into();
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::HOST, AUTHORITY)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::from(Bytes::from(body)))
            .map_err(|e| MediaProxyError::Transport(e.to_string()))?;

        let resp = self.send(request, REQUEST_TIMEOUT).await?;
        self.read_json::<ResolvedActor>(resp).await
    }

    /// `POST /v1/image/sanitize` — 受け取ったバイト列を再エンコードして返す。
    /// M7 (アップロード) で使う想定。
    pub async fn sanitize_image(
        &self,
        bytes: Bytes,
        variant: &str,
    ) -> Result<ProcessedImage, MediaProxyError> {
        let path = format!("/v1/image/sanitize?variant={variant}");
        let uri: http::Uri = UnixUri::new(&self.socket, &path).into();
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::HOST, AUTHORITY)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Full::from(bytes))
            .map_err(|e| MediaProxyError::Transport(e.to_string()))?;

        let resp = self.send(request, REQUEST_TIMEOUT).await?;
        self.read_processed(resp).await
    }

    /// `POST /v1/video/sanitize` — 受け取った動画バイト列のコンテナ
    /// メタデータを無害化して返す (再エンコードはしない)。
    pub async fn sanitize_video(&self, bytes: Bytes) -> Result<ProcessedVideo, MediaProxyError> {
        let uri: http::Uri = UnixUri::new(&self.socket, "/v1/video/sanitize").into();
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::HOST, AUTHORITY)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Full::from(bytes))
            .map_err(|e| MediaProxyError::Transport(e.to_string()))?;

        let resp = self.send(request, VIDEO_REQUEST_TIMEOUT).await?;
        self.read_processed_video(resp).await
    }

    async fn send(
        &self,
        request: Request<Full<Bytes>>,
        timeout: Duration,
    ) -> Result<hyper::Response<Incoming>, MediaProxyError> {
        let fut = self.inner.request(request);
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(MediaProxyError::Transport(e.to_string())),
            Err(_) => Err(MediaProxyError::Timeout(timeout)),
        }
    }

    /// JSON 本文を返す系のエンドポイント (`/v1/webfinger/resolve` 等) を読む。
    ///
    /// `read_processed` と違って画像バイト列は期待せず、成功時は
    /// `T` にデシリアライズする。失敗時は `Upstream` に詰める ──
    /// `error / reason` 構造は image 系と同じ。
    async fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        resp: hyper::Response<Incoming>,
    ) -> Result<T, MediaProxyError> {
        let status = resp.status();
        let bytes = read_limited_body(resp.into_body(), MAX_BODY_BYTES).await?;

        if !status.is_success() {
            let parsed: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            return Err(MediaProxyError::Upstream {
                status,
                reason: parsed
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                message: parsed
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }

        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| MediaProxyError::Transport(format!("decode json body: {e}")))
    }

    async fn read_processed(
        &self,
        resp: hyper::Response<Incoming>,
    ) -> Result<ProcessedImage, MediaProxyError> {
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let width = header_u32(resp.headers(), "x-output-width");
        let height = header_u32(resp.headers(), "x-output-height");
        let bytes = read_limited_body(resp.into_body(), MAX_BODY_BYTES).await?;

        if !status.is_success() {
            // media-proxy のエラーは JSON 構造化。`{error, reason}` を抜き出す。
            let parsed: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            return Err(MediaProxyError::Upstream {
                status,
                reason: parsed
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                message: parsed
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }

        let content_type = content_type.ok_or(MediaProxyError::MissingContentType)?;
        Ok(ProcessedImage {
            bytes,
            content_type,
            width,
            height,
        })
    }

    async fn read_processed_video(
        &self,
        resp: hyper::Response<Incoming>,
    ) -> Result<ProcessedVideo, MediaProxyError> {
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let width = header_u32(resp.headers(), "x-output-width");
        let height = header_u32(resp.headers(), "x-output-height");
        let duration_ms = header_u64(resp.headers(), "x-output-duration-ms");
        let bytes = read_limited_body(resp.into_body(), MAX_VIDEO_BODY_BYTES).await?;

        if !status.is_success() {
            let parsed: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            return Err(MediaProxyError::Upstream {
                status,
                reason: parsed
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                message: parsed
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }

        let content_type = content_type.ok_or(MediaProxyError::MissingContentType)?;
        Ok(ProcessedVideo {
            bytes,
            content_type,
            width,
            height,
            duration_ms,
        })
    }
}

/// HTTP ヘッダから `u32` を引き出す。欠如 / パース失敗時は `None`。
fn header_u32(headers: &http::HeaderMap, name: &str) -> Option<u32> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// HTTP ヘッダから `u64` を引き出す。欠如 / パース失敗時は `None`。
fn header_u64(headers: &http::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

async fn read_limited_body(body: Incoming, limit: usize) -> Result<Bytes, MediaProxyError> {
    let limited = Limited::new(body, limit);
    match limited.collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(err) => {
            // Limited は上限超過時に `Box<dyn Error>` を返す。文字列で
            // 識別子マッチするしかないので「body length」を含むものを TooLarge
            // にする。それ以外は Transport。
            let msg = err.to_string();
            if msg.contains("length limit") || msg.contains("body length") {
                Err(MediaProxyError::TooLarge)
            } else {
                Err(MediaProxyError::Transport(format!("read body: {msg}")))
            }
        }
    }
}
