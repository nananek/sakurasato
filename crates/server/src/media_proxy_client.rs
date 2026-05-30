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
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

/// 1 リクエスト全体の timeout。media-proxy 側は 15s で external GET を切る
/// (= `media_proxy::http_client::REQUEST_TIMEOUT`)、ここはその上に裕度を足した値。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// レスポンス本文の上限 (= 4 MiB)。media-proxy 経由のアバター/プレビューは
/// 通常 256 KiB 以下、最大でも `max_pixels` から逆算して数 MiB に収まる。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

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

/// fetch / sanitize の正常レスポンス。
#[derive(Debug)]
pub struct ProcessedImage {
    /// 変換後のバイト列。常に WebP。
    pub bytes: Bytes,
    /// `Content-Type` ヘッダ。常に `image/webp` だが、media-proxy 側の挙動
    /// 変更に追従できるよう型に持つ。
    pub content_type: String,
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

        let resp = self.send(request).await?;
        self.read_processed(resp).await
    }

    /// `POST /v1/image/sanitize` — 受け取ったバイト列を再エンコードして返す。
    /// M7 (アップロード) で使う想定。
    #[allow(dead_code)] // M7 で server::routes::upload から呼ぶ。
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

        let resp = self.send(request).await?;
        self.read_processed(resp).await
    }

    async fn send(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<Incoming>, MediaProxyError> {
        let fut = self.inner.request(request);
        match tokio::time::timeout(REQUEST_TIMEOUT, fut).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(MediaProxyError::Transport(e.to_string())),
            Err(_) => Err(MediaProxyError::Timeout(REQUEST_TIMEOUT)),
        }
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
        let bytes = read_limited_body(resp.into_body()).await?;

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
        })
    }
}

async fn read_limited_body(body: Incoming) -> Result<Bytes, MediaProxyError> {
    let limited = Limited::new(body, MAX_BODY_BYTES);
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
