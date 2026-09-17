//! `url`/`variant` クエリを media-proxy に橋渡しする HTTP ハンドラの共通コア。
//!
//! 2 箇所から使われる:
//!
//! - `GET /api/v1/media/proxy` ([`crate::local_api::media_proxy`]) ── TUI 用。
//!   `/api/v1/*` 全体にかかる Bearer 認証の内側。
//! - `GET /media-proxy` ([`crate::routes::media_proxy`]) ── `MiAuth` 経路
//!   ([`crate::miauth::conv`]) が返す `avatarUrl` / `bannerUrl` / 添付 `url` /
//!   `thumbnailUrl` / 本文 `:emoji:` 画像の実体。Misskey 互換クライアント
//!   (Aria 等) の画像ウィジェットは任意ヘッダを付けずに直接この URL を GET
//!   するため、**無認証** かつ **`/media/<key>` と同じ公開 AP host
//!   (`config.server.host`) 上** に置く必要がある (= `MiAuth` 専用 socket は
//!   Tailscale tailnet 越しでしか到達できない構成が推奨で、host が食い違う
//!   と画像が読み込めない。`MiAuth` の全 handler が emoji/添付 URL を一貫して
//!   `config.server.host` で組み立てている既存慣習 (`local_api::media::build_media_url`)
//!   に合わせる)。
//!
//! 無認証で公開する以上「任意 URL を無制限に fetch できる open relay」化を
//! 避ける必要がある。SSRF ガード ([`sakurasato_core::net_guard::host_blocked`])
//! に加え、呼び出し側 ([`crate::routes::media_proxy`]) が宛先 host ごとの
//! per-domain レート制限を掛ける ── [`crate::state::AppState::try_acquire_fetch`]
//! (AP object fetch 用) とは **別バケット** を使い、この公開エンドポイントへの
//! flood が正規の AP fetch 用トークンを枯渇させないようにする。
//!
//! ## レスポンス
//!
//! - 成功: `200 OK`、`Content-Type` は media-proxy が返した値 (`image/webp`)、
//!   本文は変換後のバイト列。`Cache-Control: public, max-age=86400,
//!   immutable` を付ける (= 同じ URL は短時間で内容が変わらない前提)。
//! - 失敗: media-proxy の `reason` を見て分類 ([`map_proxy_error`]):
//!   - `invalid_url` / `unsupported_scheme` / `empty_body` → `400`
//!   - `loopback` / `private` / 等 SSRF 系 → `400` (= クライアントの責)
//!   - `too_large` → `413`
//!   - `unsupported_format` / `decode_failed` 等 → `502` (= 上流が壊れている)
//!   - `upstream_http` / `transport` / `connect` → `502`
//!   - `upstream_timeout` → `504`
//!   - その他 → `502`
//!
//! `url`/`variant` の scheme / SSRF / variant 検証は [`validate`] が本サーバ側
//! でも先に行う (media-proxy も同じ検査をするが、ここで弾ければ UDS 往復を
//! 1 回節約できる多層防御)。加えて公開ルートは [`reject_self_host`] で
//! 自ホスト宛 URL (自己プロキシ連鎖) を拒否する ([`reject_self_host`] の doc
//! を参照)。
//!
//! ## クエリパラメータ
//!
//! - `url` (必須): 取得対象の絶対 URL。`http`/`https` のみ受理。
//! - `variant` (任意、既定 `avatar`): `avatar` / `emoji` / `thumbnail` /
//!   `preview` / `header` のいずれか (media-proxy 側の `Variant` と同じ表記)。

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::net_guard::{host_blocked, is_self_host};
use serde::Deserialize;
use serde_json::json;

use crate::media_proxy_client::MediaProxyError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ProxyQuery {
    pub url: String,
    #[serde(default = "default_variant")]
    pub variant: String,
}

fn default_variant() -> String {
    "avatar".to_string()
}

/// media-proxy 側 [`Variant`] にマップできる値だけ通す。
pub(crate) fn validate_variant(v: &str) -> bool {
    matches!(v, "avatar" | "emoji" | "thumbnail" | "preview" | "header")
}

/// クエリを検証し、パース済み URL を返す。シンタックス / scheme / SSRF /
/// variant のいずれかで弾かれたら `Err(400 Response)`。
pub(crate) fn validate(q: &ProxyQuery, allow_private: bool) -> Result<url::Url, Response> {
    let parsed = url::Url::parse(&q.url).map_err(|e| error_400(format!("invalid url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(error_400(format!(
            "unsupported scheme {:?}",
            parsed.scheme()
        )));
    }
    if !allow_private && let Some(reason) = host_blocked(&parsed) {
        return Err(error_400(format!(
            "host {:?} blocked: {reason}",
            parsed.host_str().unwrap_or("")
        )));
    }
    if !validate_variant(&q.variant) {
        return Err(error_400(format!("invalid variant {:?}", q.variant)));
    }
    Ok(parsed)
}

/// 検証済み URL の host が自インスタンスの公開 AP host と一致するなら拒否する
/// (E-8)。
///
/// 公開 (無認証) `GET /media-proxy` は media-proxy コンテナに任意 URL を
/// fetch させる。`host_blocked` は自ホストの公開ドメインを遮断しないため、
/// `/media-proxy?url=https://<自ホスト>/media-proxy?url=...` のような
/// **自己プロキシ連鎖** が成立してしまう (egress → インターネット → 自ホスト
/// の hairpin。per-domain レート制限と同時実行上限で増幅は有限だが、
/// media-proxy のスロットと実帯域を無駄に消費する)。
///
/// [`crate::remote_actor::enforce_url_policy`] / [`crate::delivery`] と同様に
/// **`allow_private` とは独立に常に効かせる** (自己 fetch はテスト /
/// 連合テストモードでも許可しない、という既存の横断方針に合わせる)。
///
/// 適用先は 2026-09 時点では公開 `GET /media-proxy` のみ。TUI 用
/// `/api/v1/media/proxy` は自ホスト `/media/<key>` (ローカル actor の
/// アバター・添付・絵文字) を表示する正当用途があり、TUI は画像バイト列を
/// すべてこの経路で受け取る設計のため、無条件適用するとローカルメディアが
/// 描画できなくなる。適用範囲の見直しは別途 follow-up。
///
/// **既知の限界 (2026-09 レビュー)**: 本チェックは `server_host` との文字列
/// 一致 ([`is_self_host`]) のみで、IP リテラルでの自己参照は検出しない
/// (`host_blocked` も public IP は private/loopback 判定に掛からず通す)。
/// 理論上 `url=http://<自ホストの公開 IP>/media-proxy?...` で同じ自己
/// プロキシ連鎖を再現できる余地は残る。ただし本プロジェクトが唯一公式に
/// サポートするデプロイ構成 (`DEPLOYMENT.md` §0/§4, Cloudflare Tunnel) では
/// `server` に `ports:` を一切開けず (本番 `docker-compose.yml` に該当行なし)
/// 自宅/VPS の実 IP を外部公開しない設計のため、攻撃者がこの IP に到達する
/// 経路自体が存在しない。docker 内部ネットワーク経由の代替 (compose サービス
/// 名 `server` や internal ネットの private IP) も `host_blocked` の
/// single-label-host 判定 / private IP 判定が別途遮断済み (本関数とは独立)。
/// 受信側で Host ヘッダを `server_host` と照合する検証層は現状どこにも無い
/// ことも確認済みだが、追加するなら `release-validation.yml` の
/// `stack-smoke` が `config.server.host = "localhost"` のまま
/// `curl http://127.0.0.1:8080/...` で probe している点との非互換に注意
/// (素朴な完全一致では既存 CI を壊す)。Cloudflare Tunnel を介さない直接公開
/// デプロイを新たにサポートする場合はこの前提が崩れるため、その時点で
/// 再評価すること。
pub(crate) fn reject_self_host(parsed: &url::Url, server_host: &str) -> Result<(), Response> {
    if is_self_host(parsed, server_host) {
        return Err(error_400(format!(
            "host {:?} is blocked: self-host",
            parsed.host_str().unwrap_or("")
        )));
    }
    Ok(())
}

/// 検証済み URL を media-proxy 経由で取得し、レスポンスに変換する。
pub(crate) async fn fetch_and_respond(state: &AppState, url: &str, variant: &str) -> Response {
    match state.media_proxy().fetch_image(url, variant).await {
        Ok(processed) => {
            let mut response =
                (StatusCode::OK, axum::body::Body::from(processed.bytes)).into_response();
            let headers = response.headers_mut();
            if let Ok(ct) = HeaderValue::from_str(&processed.content_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400, immutable"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(err) => map_proxy_error(&err),
    }
}

fn map_proxy_error(err: &MediaProxyError) -> Response {
    match err {
        MediaProxyError::Timeout(_) => error_status(StatusCode::GATEWAY_TIMEOUT, err.to_string()),
        MediaProxyError::Transport(_) | MediaProxyError::MissingContentType => {
            tracing::warn!(?err, "media-proxy transport failure");
            error_status(StatusCode::BAD_GATEWAY, err.to_string())
        }
        MediaProxyError::TooLarge => error_status(StatusCode::PAYLOAD_TOO_LARGE, err.to_string()),
        MediaProxyError::Upstream {
            status,
            reason,
            message,
        } => {
            // media-proxy 側のステータスを基本に転送するが、SSRF 系 (403) は
            // クライアントの責で 400 に丸める ── 「上流が拒否」ではなく
            // 「URL が無効」と見せる方が自然。
            let mapped = match status.as_u16() {
                400 | 403 => StatusCode::BAD_REQUEST,
                413 => StatusCode::PAYLOAD_TOO_LARGE,
                504 => StatusCode::GATEWAY_TIMEOUT,
                // 415 (= upstream が想定外の format を返した) も含めて、
                // それ以外は本サーバから見ると「上流壊れ」なので 502。
                _ => StatusCode::BAD_GATEWAY,
            };
            error_status(mapped, format!("{reason}: {message}"))
        }
    }
}

pub(crate) fn error_400(message: impl Into<String>) -> Response {
    error_status(StatusCode::BAD_REQUEST, message)
}

pub(crate) fn error_status(status: StatusCode, message: impl Into<String>) -> Response {
    let body = axum::Json(json!({"error": message.into()}));
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_validation() {
        for v in ["avatar", "emoji", "thumbnail", "preview", "header"] {
            assert!(validate_variant(v), "{v}");
        }
        assert!(!validate_variant(""));
        assert!(!validate_variant("AVATAR"));
        assert!(!validate_variant("banner"));
        assert!(!validate_variant("../etc/passwd"));
    }

    #[test]
    fn private_host_requires_explicit_test_opt_in() {
        let query = ProxyQuery {
            url: "http://media-host/avatar.png".into(),
            variant: "avatar".into(),
        };
        assert!(validate(&query, false).is_err());
        assert!(validate(&query, true).is_ok());
    }

    #[test]
    fn reject_self_host_matches_canonicalized_host() {
        // 大文字 / 末尾ドット違いでも自ホストとして拒否する
        // (net_guard::canonical_host の正規化に依存)。
        let query = ProxyQuery {
            url: "https://EXAMPLE.test./media/a.webp".into(),
            variant: "avatar".into(),
        };
        let parsed = validate(&query, false).unwrap();
        assert!(reject_self_host(&parsed, "example.test").is_err());
    }

    #[test]
    fn reject_self_host_allows_other_hosts() {
        let query = ProxyQuery {
            url: "https://remote.test/media/a.webp".into(),
            variant: "avatar".into(),
        };
        let parsed = validate(&query, false).unwrap();
        assert!(reject_self_host(&parsed, "example.test").is_ok());
    }

    #[test]
    fn reject_self_host_is_independent_of_allow_private() {
        // テスト用 opt-in (allow_private) が立っていても自己 fetch は拒否する
        // ── remote_actor::enforce_url_policy / delivery と同じ横断方針。
        let query = ProxyQuery {
            url: "http://example.test/media/a.webp".into(),
            variant: "avatar".into(),
        };
        let parsed = validate(&query, true).unwrap();
        assert!(reject_self_host(&parsed, "example.test").is_err());
    }
}
