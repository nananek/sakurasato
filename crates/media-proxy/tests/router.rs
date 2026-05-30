//! `crates/media-proxy` の router 統合テスト。
//!
//! `tower::ServiceExt::oneshot` で `Router` を直接叩く。実 socket / 実外向き
//! HTTP は出さない (= `/v1/image/fetch` は 403/400 が返る経路だけ検証する)。
//! 画像変換本体は `image_pipeline` の unit test と、本ファイルの
//! `/v1/image/sanitize` 経路 (= 既知の PNG を入れて WebP が返る) で覆う。

#![forbid(unsafe_code)]

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use image::{ImageBuffer, ImageFormat, Rgba};
use sakurasato_core::Config;
use sakurasato_core::config::{DatabaseConfig, MediaProxyConfig, ServerConfig, StorageConfig};
use sakurasato_media_proxy::ProxyState;
use serde_json::Value;
use tower::ServiceExt;

fn make_config() -> Config {
    Config {
        server: ServerConfig {
            host: "example.test".into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/x".into(),
            user: "me".into(),
        },
        database: DatabaseConfig {
            url: "unused".into(),
            password_file: None,
        },
        storage: StorageConfig {
            endpoint: "http://localhost".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            secret_access_key_file: None,
        },
        media_proxy: MediaProxyConfig {
            socket: "/tmp/media.sock".into(),
            // 4 MiB 上限。アバター用テスト PNG は 100x80 で十分小さい。
            max_bytes: 4 * 1024 * 1024,
            max_pixels: 16_000_000,
        },
    }
}

fn make_router() -> axum::Router {
    let state = ProxyState::from_config(make_config()).expect("ProxyState build");
    sakurasato_media_proxy::router(state)
}

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
        #[allow(clippy::cast_possible_truncation)]
        Rgba([(x % 256) as u8, (y % 256) as u8, 0, 255])
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    bytes
}

async fn read_json(resp: axum::response::Response) -> Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn read_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = make_router();
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_bytes(resp).await;
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn fetch_rejects_invalid_url() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::post("/v1/image/fetch")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"url":"not a url","variant":"avatar"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = read_json(resp).await;
    assert_eq!(json["reason"], "invalid_url");
}

#[tokio::test]
async fn fetch_rejects_non_http_scheme() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::post("/v1/image/fetch")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"url":"file:///etc/passwd","variant":"avatar"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = read_json(resp).await;
    assert_eq!(json["reason"], "unsupported_scheme");
}

#[tokio::test]
async fn fetch_blocks_ssrf_targets() {
    let app = make_router();
    for url in [
        "http://127.0.0.1/x",
        "http://10.0.0.1/x",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost/x",
        "http://postgres.local/x",
        "http://[::1]/x",
    ] {
        let body = format!(r#"{{"url":"{url}","variant":"avatar"}}"#);
        let req = Request::post("/v1/image/fetch")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{url}");
        let json = read_json(resp).await;
        // reason は loopback / private / link-local / localhost-domain /
        // mdns-local 等 ── net_guard の戻り値そのまま。
        assert!(
            !json["reason"].as_str().unwrap_or("").is_empty(),
            "{url} → {json}"
        );
    }
}

#[tokio::test]
async fn fetch_rejects_malformed_body() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::post("/v1/image/fetch")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    // axum::Json は parse 失敗で 400 を返す (ステータスは axum 既定の挙動)。
    assert!(
        resp.status().is_client_error(),
        "expected 4xx got {}",
        resp.status()
    );
}

#[tokio::test]
async fn sanitize_round_trips_png_to_webp() {
    let app = make_router();
    let png = png_bytes(150, 100);
    let resp = app
        .oneshot(
            Request::post("/v1/image/sanitize?variant=avatar")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(png))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(ct, "image/webp");
    let bytes = read_bytes(resp).await;
    assert!(!bytes.is_empty());
    // WebP magic: bytes 0..4 = "RIFF", 8..12 = "WEBP".
    assert_eq!(&bytes[0..4], b"RIFF", "expected RIFF prefix");
    assert_eq!(&bytes[8..12], b"WEBP", "expected WEBP magic");
}

#[tokio::test]
async fn sanitize_rejects_empty_body() {
    let app = make_router();
    let resp = app
        .oneshot(
            Request::post("/v1/image/sanitize?variant=avatar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = read_json(resp).await;
    assert_eq!(json["reason"], "empty_body");
}

#[tokio::test]
async fn sanitize_rejects_invalid_variant() {
    let app = make_router();
    let png = png_bytes(50, 50);
    let resp = app
        .oneshot(
            Request::post("/v1/image/sanitize?variant=banner")
                .body(Body::from(png))
                .unwrap(),
        )
        .await
        .unwrap();
    // Query 検査は axum::Query → serde で行われ、未知 variant は 400 になる。
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn sanitize_resizes_oversized_avatar() {
    let app = make_router();
    let png = png_bytes(1024, 800);
    let resp = app
        .oneshot(
            Request::post("/v1/image/sanitize?variant=avatar")
                .body(Body::from(png))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let width = resp
        .headers()
        .get("x-output-width")
        .unwrap()
        .to_str()
        .unwrap();
    let height = resp
        .headers()
        .get("x-output-height")
        .unwrap()
        .to_str()
        .unwrap();
    let w: u32 = width.parse().unwrap();
    let h: u32 = height.parse().unwrap();
    assert!(w <= 256, "width {w} > 256");
    assert!(h <= 256, "height {h} > 256");
}
