//! `LocalApi` の Unix socket クライアントとして最低限のヘッダ/ボディ往復を検証。
//!
//! Postgres を立てずに済むよう、本テストは axum + tokio で生 UDS リスナを
//! 1 本立て、固定 JSON を返す薄いハンドラだけ書く。`server` クレートに依存
//! せず、wire-format の整合性 (Bearer / Accept / JSON body) を確認する。

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use sakurasato_tui::client::{CreateNoteRequest, Endpoint, LocalApi};
use serde_json::json;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Mutex;

#[derive(Default)]
struct CapturedAuth {
    inner: Mutex<Option<String>>,
}

#[tokio::test]
async fn whoami_passes_bearer_and_parses_response() -> anyhow::Result<()> {
    let (socket, captured, _server) = spawn_server().await?;
    let api = LocalApi::from_socket(socket.clone(), "secret-token".into());
    let whoami = api.whoami().await.context("whoami")?;
    assert_eq!(whoami.preferred_username, "me");
    assert_eq!(whoami.host, "x.test");
    let auth = captured.inner.lock().await.clone();
    assert_eq!(auth, Some("Bearer secret-token".into()));
    Ok(())
}

#[tokio::test]
async fn timeline_home_returns_parsed_notes() -> anyhow::Result<()> {
    let (socket, _captured, _server) = spawn_server().await?;
    let api = LocalApi::from_socket(socket, "t".into());
    let resp = api.timeline_home(None, 5).await?;
    assert_eq!(resp.notes.len(), 1);
    assert_eq!(resp.notes[0].content, "hello");
    Ok(())
}

#[tokio::test]
async fn create_note_round_trips() -> anyhow::Result<()> {
    let (socket, _captured, _server) = spawn_server().await?;
    let api = LocalApi::from_socket(socket, "t".into());
    let req = CreateNoteRequest {
        content: "ping".into(),
        summary: None,
        visibility: Some("public".into()),
        sensitive: Some(false),
        language: None,
        in_reply_to_ap_id: None,
        attachment_ids: Vec::new(),
    };
    let resp = api.create_note(&req).await?;
    assert_eq!(resp.id, 99);
    assert_eq!(resp.queued_deliveries, 0);
    Ok(())
}

/// **#69**: TCP モードでも UDS と同じく Bearer + JSON 往復が成立すること。
/// Tailscale tailnet 経由で別端末から TUI を動かすケースの回帰テスト。
#[tokio::test]
async fn whoami_via_tcp_backend_round_trips() -> anyhow::Result<()> {
    let (addr, captured, _server) = spawn_tcp_server().await?;
    let api = LocalApi::new(
        Endpoint::Tcp {
            base: format!("http://{addr}"),
        },
        "tcp-token".into(),
    );
    let whoami = api.whoami().await.context("whoami over tcp")?;
    assert_eq!(whoami.preferred_username, "me");
    assert_eq!(whoami.host, "x.test");
    let auth = captured.inner.lock().await.clone();
    assert_eq!(auth, Some("Bearer tcp-token".into()));
    Ok(())
}

#[tokio::test]
async fn http_400_is_surfaced_as_status_error() -> anyhow::Result<()> {
    let (socket, _captured, _server) = spawn_server().await?;
    let api = LocalApi::from_socket(socket, "t".into());
    // 既知の bad path → 400 を返す。
    let req = CreateNoteRequest {
        content: "BAD".into(),
        summary: None,
        visibility: Some("public".into()),
        sensitive: None,
        language: None,
        in_reply_to_ap_id: None,
        attachment_ids: Vec::new(),
    };
    let err = api.create_note(&req).await.unwrap_err();
    assert!(format!("{err}").contains("400"), "expected 400 in {err}");
    Ok(())
}

/// テストごとに掴むサーバハンドル一式。`_dir` は `TempDir` の生存を保持する
/// ためだけのフィールドで、drop されると socket file ごと消える。
/// 以前は `Box::leak` していたが、`--test-threads` を上げると leak が累積
/// するため `Arc<TempDir>` で明示的に lifetime を縛る形に直した
/// ([PR #34 claude-review 改善提案 #1])。
#[allow(dead_code, reason = "TempDir/JoinHandle を握っておくだけで使わない")]
struct ServerGuard {
    dir: Arc<tempfile::TempDir>,
    handle: tokio::task::JoinHandle<()>,
}

async fn spawn_server() -> anyhow::Result<(PathBuf, Arc<CapturedAuth>, ServerGuard)> {
    let dir = Arc::new(tempfile::tempdir()?);
    let socket = dir.path().join("api.sock");

    let captured = Arc::new(CapturedAuth::default());
    let listener = UnixListener::bind(&socket).context("bind uds")?;
    let cap_for_task = captured.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                break;
            };
            let captured = cap_for_task.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service =
                    service_fn(move |req: Request<Incoming>| handle(req, captured.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    // ソケット readiness 安定化。tokio bind 直後でも accept は OK だが、
    // CI 環境の小さなずれを吸収する。
    tokio::time::sleep(Duration::from_millis(20)).await;
    Ok((socket, captured, ServerGuard { dir, handle }))
}

/// `spawn_server` の TCP 版。`127.0.0.1:0` で bind して OS 割当ポートを返す。
async fn spawn_tcp_server()
-> anyhow::Result<(std::net::SocketAddr, Arc<CapturedAuth>, TcpServerGuard)> {
    let captured = Arc::new(CapturedAuth::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.context("bind tcp")?;
    let addr = listener.local_addr()?;
    let cap_for_task = captured.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let captured = cap_for_task.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service =
                    service_fn(move |req: Request<Incoming>| handle(req, captured.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    Ok((addr, captured, TcpServerGuard { handle }))
}

#[allow(dead_code, reason = "JoinHandle を握っておくだけで使わない")]
struct TcpServerGuard {
    handle: tokio::task::JoinHandle<()>,
}

async fn handle(
    req: Request<Incoming>,
    captured: Arc<CapturedAuth>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if let Some(auth) = req.headers().get(AUTHORIZATION) {
        let s = auth.to_str().unwrap_or("").to_string();
        *captured.inner.lock().await = Some(s);
    }
    let path = req.uri().path().to_string();
    let body = req
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let resp = match path.as_str() {
        "/api/v1/whoami" => json_response(
            StatusCode::OK,
            json!({
                "ap_id": "https://x.test/users/me",
                "preferred_username": "me",
                "host": "x.test",
                "display_name": null,
                "summary": null,
                "icon_url": null,
                "image_url": null,
                "inbox": "https://x.test/users/me/inbox",
                "outbox": null
            }),
        ),
        "/api/v1/timeline/home" => json_response(
            StatusCode::OK,
            json!({
                "notes": [{
                    "id": 1,
                    "ap_id": "https://x.test/notes/1",
                    "url": "https://x.test/notes/1",
                    "actor_id": 1,
                    "actor_ap_id": "https://x.test/users/me",
                    "actor_preferred_username": "me",
                    "actor_display_name": null,
                    "content": "hello",
                    "summary": null,
                    "language": null,
                    "visibility": "public",
                    "sensitive": false,
                    "in_reply_to_ap_id": null,
                    "in_reply_to_note_id": null,
                    "published_at": "2026-05-30T12:34:56Z",
                    "is_local": true
                }],
                "next_before_id": 1
            }),
        ),
        "/api/v1/notes" => {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content == "BAD" {
                json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "content rejected for test"}),
                )
            } else {
                json_response(
                    StatusCode::CREATED,
                    json!({
                        "id": 99,
                        "ap_id": "https://x.test/notes/99",
                        "url": "https://x.test/notes/99",
                        "content": content,
                        "summary": null,
                        "visibility": "public",
                        "sensitive": false,
                        "published_at": "2026-05-30T12:34:56Z",
                        "queued_deliveries": 0
                    }),
                )
            }
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::new()))
            .unwrap(),
    };
    Ok(resp)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "json! マクロからそのまま渡しやすいよう値受け"
)]
fn json_response(status: StatusCode, body: serde_json::Value) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&body).expect("json");
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .expect("build response")
}
