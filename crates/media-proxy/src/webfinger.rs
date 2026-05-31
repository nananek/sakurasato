//! `POST /v1/webfinger/resolve` — `acct:user@host` から `ActivityPub` actor URI を解決する (M10)。
//!
//! # 期待リクエスト
//!
//! ```json
//! { "acct": "alice@example.com" }
//! ```
//!
//! - `acct:` プレフィクスはあっても無くてもよい (両方受ける)。
//! - 先頭 `@` も剥がす (TUI / CLI の入力ゆれを吸収)。
//!
//! # 期待レスポンス
//!
//! ```json
//! {
//!   "subject": "acct:alice@example.com",
//!   "actor_uri": "https://example.com/users/alice",
//!   "aliases": ["https://example.com/@alice"]
//! }
//! ```
//!
//! - `actor_uri` は `links[]` の中で `rel == "self"` かつ
//!   `type` が `application/activity+json` または
//!   `application/ld+json` (`profile="https://www.w3.org/ns/activitystreams"`)
//!   のものの `href`。**最初に一致したもの** を返す。
//! - 無ければ `404` (`reason = "no_self_link"`)。
//!
//! # 設計
//!
//! - `WebFinger` / actor JSON 取得は **画像デコードを伴わない JSON 通信** だが、
//!   M10 で server コンテナの egress を絞るため、外向き接続を media-proxy に
//!   寄せる。SSRF / redirect / `max_bytes` は既存 `http_client` の防御をそのまま
//!   流用する。
//! - 名前解決の最終段は host のみ ── `host_blocked` は `Url` を受けるので、
//!   組み立てた `https://<host>/.well-known/webfinger?...` URL でチェックする。
//! - 本エンドポイントは actor JSON を **取得しない** ── そこは server 側の
//!   `remote_actor::fetch_and_upsert` が HTTP 署名付きでやる仕事 (= 我々の
//!   sender 鍵で署名するため、media-proxy には鍵を渡せない)。`WebFinger` は
//!   一般に署名不要なのでここで完結できる。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use sakurasato_core::net_guard::host_blocked;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use url::Url;

use crate::error::ApiError;
use crate::state::ProxyState;

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    /// `acct:` プレフィクスはあっても無くても可。先頭 `@` も剥がす。
    pub acct: String,
}

#[derive(Debug, Serialize)]
pub struct ResolveResponse {
    /// 正規化された `acct:user@host` (`acct:` プレフィクス付き)。
    pub subject: String,
    /// `ActivityPub` actor の URI (`rel=self` + AS2 type の `href`)。
    pub actor_uri: String,
    /// `WebFinger` レスポンスの `aliases[]` をそのまま転載。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

pub async fn handle(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<ResolveRequest>,
) -> Response {
    match resolve_inner(&state, &req).await {
        Ok(resp) => (axum::http::StatusCode::OK, Json(resp)).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn resolve_inner(
    state: &ProxyState,
    req: &ResolveRequest,
) -> Result<ResolveResponse, ApiError> {
    let (user, host) = parse_acct(&req.acct)?;
    let resource = format!("acct:{user}@{host}");

    // `WebFinger` は仕様上 https のみ ([RFC 7033 §4.2])。
    // `host` は parse_acct で軽くサニタイズ済みだが、最終 URL を組んでから
    // SSRF / scheme 検査を通す。
    let endpoint = format!("https://{host}/.well-known/webfinger");
    let mut url = Url::parse(&endpoint)
        .map_err(|e| ApiError::bad_request("invalid_host", format!("parse url: {e}")))?;
    url.query_pairs_mut().append_pair("resource", &resource);

    if let Some(reason) = host_blocked(&url) {
        return Err(ApiError::blocked(
            reason,
            format!(
                "host {:?} is blocked: {reason}",
                url.host_str().unwrap_or("")
            ),
        ));
    }

    let body = download_webfinger(state, &url).await?;
    let json: JsonValue = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("invalid_json", format!("parse webfinger json: {e}")))?;

    let actor_uri = extract_self_link(&json).ok_or_else(|| ApiError {
        status: axum::http::StatusCode::NOT_FOUND,
        reason: "no_self_link",
        message: format!("webfinger for {resource} has no AP self link"),
    })?;

    let aliases = json
        .get("aliases")
        .and_then(JsonValue::as_array)
        .map(|a| {
            a.iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(ResolveResponse {
        subject: resource,
        actor_uri,
        aliases,
    })
}

/// `acct:user@host` / `@user@host` / `user@host` のいずれも受ける。
/// 返すのはサニタイズ済みの `(user, host)`。
///
/// `host` は ASCII の `[A-Za-z0-9.-]` と `:` (port) のみ許可する ── これは
/// URL に組み立てる前のサニタイズで、IDN ホストは未対応 (Fediverse の
/// 実用上ほぼ全件 ASCII で配送される)。
fn parse_acct(input: &str) -> Result<(String, String), ApiError> {
    let trimmed = input.trim();
    let body = trimmed
        .strip_prefix("acct:")
        .unwrap_or(trimmed)
        .trim_start_matches('@');
    let (user, host) = body.split_once('@').ok_or_else(|| {
        ApiError::bad_request("invalid_acct", format!("expected user@host, got {body:?}"))
    })?;

    if user.is_empty() || host.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_acct",
            format!("user / host must be non-empty in {body:?}"),
        ));
    }

    // user 部の文字種ゆるめ (Mastodon は `[A-Za-z0-9_]`、Pleroma は `.` も
    // 許す)。ここでは CR/LF/タブ/制御文字と `@` の二重出現だけ弾く。
    if user.chars().any(|c| c.is_control() || c == '@' || c == '/') {
        return Err(ApiError::bad_request(
            "invalid_acct",
            format!("user part contains forbidden char: {user:?}"),
        ));
    }

    // host は ASCII の host-safe 文字のみ。`:port` も許可。
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return Err(ApiError::bad_request(
            "invalid_acct",
            format!("host part contains forbidden char: {host:?}"),
        ));
    }

    Ok((user.to_string(), host.to_string()))
}

/// `links[]` から AP-compatible な `rel="self"` を 1 件抽出する。
fn extract_self_link(json: &JsonValue) -> Option<String> {
    let links = json.get("links")?.as_array()?;
    for link in links {
        let rel = link.get("rel").and_then(JsonValue::as_str)?;
        if rel != "self" {
            continue;
        }
        let typ = link.get("type").and_then(JsonValue::as_str).unwrap_or("");
        if is_ap_link_type(typ)
            && let Some(href) = link.get("href").and_then(JsonValue::as_str)
        {
            return Some(href.to_string());
        }
    }
    None
}

/// AS2 を表現する MIME を判定する。Mastodon は `application/activity+json`、
/// Pleroma / `GoToSocial` 等は `application/ld+json; profile=...` を返す。
fn is_ap_link_type(typ: &str) -> bool {
    let lower = typ.to_ascii_lowercase();
    if lower.starts_with("application/activity+json") {
        return true;
    }
    if lower.starts_with("application/ld+json")
        && lower.contains("https://www.w3.org/ns/activitystreams")
    {
        return true;
    }
    false
}

async fn download_webfinger(state: &ProxyState, url: &Url) -> Result<Vec<u8>, ApiError> {
    let max_bytes = state.max_bytes();
    let resp = state
        .http()
        .get(url.clone())
        .header("accept", "application/jrd+json, application/json;q=0.5")
        .send()
        .await
        .map_err(|err| classify_reqwest_err("send", &err))?;

    if !resp.status().is_success() {
        return Err(ApiError::upstream(
            "upstream_http",
            format!("upstream returned {}", resp.status().as_u16()),
        ));
    }

    if let Some(len) = resp.content_length()
        && (usize::try_from(len).unwrap_or(usize::MAX)) > max_bytes
    {
        return Err(ApiError::too_large(format!(
            "Content-Length {len} exceeds max_bytes {max_bytes}",
        )));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4 * 1024);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| classify_reqwest_err("read_body", &err))?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ApiError::too_large(format!(
                "body exceeds max_bytes {max_bytes} during stream",
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn classify_reqwest_err(phase: &'static str, err: &reqwest::Error) -> ApiError {
    if err.is_timeout() {
        return ApiError::timeout(format!("{phase}: {err}"));
    }
    if err.is_redirect() {
        return ApiError::blocked("redirect_blocked", format!("{phase}: {err}"));
    }
    if err.is_connect() {
        return ApiError::upstream("connect", format!("{phase}: {err}"));
    }
    ApiError::upstream("transport", format!("{phase}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_acct_accepts_all_three_prefix_forms() {
        let (u, h) = parse_acct("acct:alice@example.com").unwrap();
        assert_eq!((u.as_str(), h.as_str()), ("alice", "example.com"));
        let (u, h) = parse_acct("@alice@example.com").unwrap();
        assert_eq!((u.as_str(), h.as_str()), ("alice", "example.com"));
        let (u, h) = parse_acct("alice@example.com").unwrap();
        assert_eq!((u.as_str(), h.as_str()), ("alice", "example.com"));
    }

    #[test]
    fn parse_acct_rejects_bad_inputs() {
        assert!(parse_acct("no-at-sign").is_err());
        assert!(parse_acct("@@double-at").is_err());
        assert!(parse_acct("a@").is_err());
        assert!(parse_acct("@b").is_err());
        assert!(parse_acct("alice@ev il.example").is_err());
        assert!(parse_acct("alice/extra@example.com").is_err());
        // 制御文字
        assert!(parse_acct("alice\n@example.com").is_err());
    }

    #[test]
    fn parse_acct_allows_port() {
        let (u, h) = parse_acct("alice@example.com:8443").unwrap();
        assert_eq!((u.as_str(), h.as_str()), ("alice", "example.com:8443"));
    }

    #[test]
    fn extract_self_link_prefers_activity_json() {
        let j = json!({
            "links": [
                {"rel": "http://webfinger.net/rel/profile-page", "href": "https://x/@alice"},
                {"rel": "self", "type": "text/html", "href": "https://x/@alice"},
                {"rel": "self", "type": "application/activity+json", "href": "https://x/users/alice"}
            ]
        });
        assert_eq!(
            extract_self_link(&j).as_deref(),
            Some("https://x/users/alice")
        );
    }

    #[test]
    fn extract_self_link_accepts_ld_json_with_as2_profile() {
        let j = json!({
            "links": [
                {
                    "rel": "self",
                    "type": "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
                    "href": "https://x/users/bob"
                }
            ]
        });
        assert_eq!(
            extract_self_link(&j).as_deref(),
            Some("https://x/users/bob")
        );
    }

    #[test]
    fn extract_self_link_rejects_plain_ld_json() {
        // AS2 profile が無い ld+json は AP として確信できないので拒否する。
        let j = json!({
            "links": [
                {"rel": "self", "type": "application/ld+json", "href": "https://x/users/c"}
            ]
        });
        assert!(extract_self_link(&j).is_none());
    }

    #[test]
    fn extract_self_link_returns_none_when_missing() {
        assert!(extract_self_link(&json!({})).is_none());
        assert!(extract_self_link(&json!({"links": []})).is_none());
        assert!(
            extract_self_link(&json!({"links": [{"rel": "other", "type": "application/activity+json", "href": "x"}]}))
                .is_none()
        );
    }
}
