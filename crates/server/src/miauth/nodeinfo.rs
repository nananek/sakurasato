//! `MiAuth` listener 用 `NodeInfo` (= #168 / 親 #150)。
//!
//! AP listener (= [`crate::routes::nodeinfo`]) も `/.well-known/nodeinfo` と
//! `/nodeinfo/2.1` を返すが、`MiAuth` 経路は **Tailscale 越しに mobile client
//! が直接叩く** ([DEPLOYMENT.md §6.3](../../../../DEPLOYMENT.md)) ので
//! `config.server.host` (= 公開 AP 用、`sakurasato.example.com` 等) を href に
//! 載せても tailscale host (= `foo.tailnet.ts.net:8443` 等) からは追跡できない。
//!
//! そのため `MiAuth` 側の discovery handler は **request の Host header** から
//! base URL を組み立てる。reverse proxy 配下なら `X-Forwarded-Host` /
//! `X-Forwarded-Proto` を優先する (= cloudflared / nginx / caddy 越しでも
//! 正しく組み立てられる、ただし `MiAuth` 経路は §6.2 でこれら public reverse
//! proxy 経由公開を禁止しているので主用途は **tailscale 直 + Host header** の
//! 経路)。
//!
//! ## v2.1 doc 本体
//!
//! 2.1 doc は AP listener と完全に共通 ── software / protocols / usage 等は
//! インスタンス全体で 1 つしか持たない (= `MiAuth` listener だけ別人格を名乗
//! るのは不正確)。実装は [`crate::routes::nodeinfo::v2_1`] を直接 mount する。
//!
//! ## AGPL discipline
//!
//! [NodeInfo](http://nodeinfo.diaspora.software/) は public spec で著作権対象外
//! (= interface)。Misskey 本体 source 不参照。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::state::AppState;

const SCHEMA_2_1: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";

#[derive(Debug, Serialize)]
pub struct Discovery {
    pub links: Vec<DiscoveryLink>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryLink {
    pub rel: String,
    pub href: String,
}

/// `GET /.well-known/nodeinfo` (= discovery)。
///
/// href は **request の Host header** から組み立てる。reverse proxy 配下は
/// `X-Forwarded-Host` + `X-Forwarded-Proto` を優先 (= 一般的 de facto)。
/// いずれも欠落していれば `config.server.host` + `https` にフォールバック。
pub async fn well_known(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = build_base_url(&headers, &state.config().server.host);
    let body = Discovery {
        links: vec![DiscoveryLink {
            rel: SCHEMA_2_1.into(),
            href: format!("{base}/nodeinfo/2.1"),
        }],
    };
    Json(body).into_response()
}

/// request → base URL (= `<scheme>://<host>`)。
///
/// 優先順:
/// 1. `X-Forwarded-Host` + `X-Forwarded-Proto` (= reverse proxy 配下)
/// 2. `Host` header + `https` (= 直接接続、tailscale serve など)
/// 3. `config.server.host` + `https` (= header が一切無い境界ケース)
///
/// ## セキュリティ前提
///
/// `X-Forwarded-Host` の値は **サニタイズせずに URL 組立てに使う**。`MiAuth`
/// listener は `DEPLOYMENT.md` §6.2 で public reverse proxy 越し公開を禁止して
/// おり、推奨経路は tailscale tailnet 限定。public reverse proxy の前に置く
/// 場合は upstream で `X-Forwarded-Host` を allowlist 化する責務が運用側に
/// ある (= 本実装で allowlist 検査を抱え込むと multi-tenant 用途の柔軟性が
/// 落ちる)。
///
/// `Host` ヘッダ単体の場合は HTTP/1.1 仕様で 1 リクエスト 1 個に縛られて
/// いるため inject 不可。tailscale serve 経由なら tailscale 側が `Host` を
/// 上書きするので攻撃面はない。
pub(crate) fn build_base_url(headers: &HeaderMap, fallback_host: &str) -> String {
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
        .unwrap_or(fallback_host);
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn forwarded_host_and_proto_take_precedence() {
        let hdrs = h(&[
            ("host", "internal:8081"),
            ("x-forwarded-host", "sakurasato.example.com"),
            ("x-forwarded-proto", "https"),
        ]);
        let base = build_base_url(&hdrs, "fallback.example");
        assert_eq!(base, "https://sakurasato.example.com");
    }

    #[test]
    fn host_header_when_no_forwarded() {
        let hdrs = h(&[("host", "foo.tailnet.ts.net:8443")]);
        let base = build_base_url(&hdrs, "fallback.example");
        // X-Forwarded-Proto 無しの場合、tailscale serve は https を終端するので
        // デフォルト https で問題ない (= 平文 HTTP 経路は実運用で稀)。
        assert_eq!(base, "https://foo.tailnet.ts.net:8443");
    }

    #[test]
    fn fallback_when_no_headers() {
        let hdrs = HeaderMap::new();
        let base = build_base_url(&hdrs, "config-host.example");
        assert_eq!(base, "https://config-host.example");
    }

    #[test]
    fn http_proto_explicit() {
        let hdrs = h(&[("host", "127.0.0.1:8081"), ("x-forwarded-proto", "http")]);
        let base = build_base_url(&hdrs, "fallback");
        assert_eq!(base, "http://127.0.0.1:8081");
    }
}
