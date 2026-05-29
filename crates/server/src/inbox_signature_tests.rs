//! M3b-2 PR1 統合テスト: `/inbox` に POST が来たときの HTTP 署名検証の
//! end-to-end 動作。
//!
//! 本物の RSA / Ed25519 鍵で署名した HTTP リクエストを組み立て、
//! `tower::ServiceExt::oneshot` で router に投入し、検証通過 (202) と
//! 各種失敗ケース (400 / 401) を網羅する。
//!
//! lib 内の `#[cfg(test)]` モジュールとして書くのは、署名計算で
//! [`crate::sign::cavage`] / [`crate::sign::rfc9421`] の crate-private な
//! 関数を直接呼びたいため。`crates/server/tests/` 配下の integration test
//! からは pub(crate) 関数が見えない。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey as EdSigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey as EdEncodePrivateKey;
use ed25519_dalek::pkcs8::EncodePublicKey as EdEncodePublicKey;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding as EdLineEnding;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use rsa::RsaPrivateKey;
use rsa::pkcs8::LineEnding as RsaLineEnding;
use rsa::rand_core::OsRng;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sqlx::PgPool;
use tower::ServiceExt;

use crate::routes::router;
use crate::sign::{cavage, digest, rfc9421};
use crate::state::AppState;

const HOST: &str = "sakura.test";
const REMOTE_HOST: &str = "nekonoverse.test";
const REMOTE_USER: &str = "carol";

fn make_config() -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: HOST.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            user: "alice".into(),
        },
        database: sakurasato_core::config::DatabaseConfig {
            url: "unused-by-tests".into(),
            password_file: None,
        },
        storage: sakurasato_core::config::StorageConfig {
            endpoint: "http://localhost".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            secret_access_key_file: None,
        },
        media_proxy: sakurasato_core::config::MediaProxyConfig {
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
        },
    }
}

fn fresh_rsa() -> (String, String) {
    // 1024-bit でテスト速度を確保 (プロダクションでは 2048+)。
    let priv_key = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
    let pub_pem = priv_key
        .to_public_key()
        .to_public_key_pem(RsaLineEnding::LF)
        .unwrap();
    let priv_pem = priv_key
        .to_pkcs8_pem(RsaLineEnding::LF)
        .unwrap()
        .to_string();
    (priv_pem, pub_pem)
}

fn fresh_ed25519() -> (String, String) {
    let sk = EdSigningKey::generate(&mut OsRng);
    let priv_pem = sk.to_pkcs8_pem(EdLineEnding::LF).unwrap().to_string();
    let pub_pem = sk
        .verifying_key()
        .to_public_key_pem(EdLineEnding::LF)
        .unwrap();
    (priv_pem, pub_pem)
}

/// テスト用に remote actor を `actor` テーブルへ insert する。
fn build_remote_actor(rsa_pub_pem: &str, ed25519_pub_pem: Option<&str>) -> NewActor {
    let ap_id = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: REMOTE_USER.into(),
        host: REMOTE_HOST.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: None,
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: rsa_pub_pem.to_string(),
        private_key_pem: None, // remote actor は秘密鍵を持たない
        ed25519_public_key_id: ed25519_pub_pem.map(|_| format!("{ap_id}#ed25519-key")),
        ed25519_public_key_pem: ed25519_pub_pem.map(str::to_string),
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: false,
        actor_type: "Person".into(),
    }
}

fn headers_to_map(req: &Request<Body>) -> HeaderMap {
    req.headers().clone()
}

/// cavage RSA-SHA256 で POST inbox リクエストを組み立てる (default covered)。
fn build_cavage_post(
    body: &[u8],
    rsa_priv_pem: &str,
    keyid: &str,
    date: &str,
    digest_override: Option<&str>,
) -> Request<Body> {
    build_cavage_post_with_covered(
        body,
        rsa_priv_pem,
        keyid,
        date,
        digest_override,
        &["(request-target)", "host", "date", "digest"],
    )
}

/// cavage の covered headers を任意に指定して POST inbox を組み立てる。
/// covered 不足のテスト (F2/F4) で使う。
fn build_cavage_post_with_covered(
    body: &[u8],
    rsa_priv_pem: &str,
    keyid: &str,
    date: &str,
    digest_override: Option<&str>,
    covered: &[&str],
) -> Request<Body> {
    let path = "/inbox";
    let digest_value = digest_override.map_or_else(|| digest::format_cavage(body), str::to_string);
    let mut req = Request::post(path)
        .header("host", HOST)
        .header("date", date)
        .header("digest", &digest_value)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let headers = headers_to_map(&req);
    let base = cavage::build_signature_base("POST", path, covered, &headers, None, None).unwrap();
    let sig_bytes = cavage::sign_rsa_sha256(base.as_bytes(), rsa_priv_pem).unwrap();
    let sig_b64 = B64.encode(sig_bytes);
    let headers_param = covered.join(" ");
    let sig_header = format!(
        "keyId=\"{keyid}\",algorithm=\"rsa-sha256\",headers=\"{headers_param}\",signature=\"{sig_b64}\""
    );
    let name = HeaderName::from_static("signature");
    req.headers_mut()
        .insert(name, HeaderValue::from_str(&sig_header).unwrap());
    req
}

/// RFC 9421 + Ed25519 で POST inbox リクエストを組み立てる (default covered)。
fn build_rfc9421_post(
    body: &[u8],
    ed25519_priv_pem: &str,
    keyid: &str,
    date: &str,
) -> Request<Body> {
    build_rfc9421_post_with_covered(
        body,
        ed25519_priv_pem,
        keyid,
        date,
        &["@method", "@target-uri", "host", "date", "content-digest"],
        None,
    )
}

/// RFC 9421 の covered components / `created` パラメタを任意に指定する版。
/// covered 不足 (F1) や `created` 異常値 (F5) のテストで使う。
fn build_rfc9421_post_with_covered(
    body: &[u8],
    ed25519_priv_pem: &str,
    keyid: &str,
    date: &str,
    covered: &[&str],
    created_override: Option<i64>,
) -> Request<Body> {
    let path = "/inbox";
    let target_uri = format!("https://{HOST}{path}");
    let content_digest = digest::format_content_digest(body);
    #[allow(clippy::cast_possible_wrap, reason = "test fixture, secs fits in i64")]
    let created_default = httpdate::parse_http_date(date)
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let created = created_override.unwrap_or(created_default);
    let mut req = Request::post(path)
        .header("host", HOST)
        .header("date", date)
        .header("content-digest", &content_digest)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let headers = headers_to_map(&req);
    let covered_quoted = covered
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let raw_value =
        format!(r#"({covered_quoted});created={created};keyid="{keyid}";alg="ed25519""#);
    let base =
        rfc9421::build_signature_base("POST", &target_uri, covered, &headers, &raw_value).unwrap();
    let sig_bytes = rfc9421::sign_ed25519(base.as_bytes(), ed25519_priv_pem).unwrap();
    let sig_b64 = B64.encode(sig_bytes);
    let input_value = format!("sig1={raw_value}");
    let sig_value = format!("sig1=:{sig_b64}:");
    req.headers_mut().insert(
        HeaderName::from_static("signature-input"),
        HeaderValue::from_str(&input_value).unwrap(),
    );
    req.headers_mut().insert(
        HeaderName::from_static("signature"),
        HeaderValue::from_str(&sig_value).unwrap(),
    );
    req
}

fn now_http_date() -> String {
    httpdate::fmt_http_date(std::time::SystemTime::now())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_rsa_valid_signature_is_accepted(pool: PgPool) {
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    let req = build_cavage_post(body, &priv_pem, &keyid, &now_http_date(), None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "cavage RSA should pass"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_ed25519_valid_signature_is_accepted(pool: PgPool) {
    let (rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();
    let _ = rsa_priv; // unused; remote actor still needs an RSA pubkey column
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#ed25519-key");
    let req = build_rfc9421_post(body, &ed_priv, &keyid, &now_http_date());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "RFC 9421 Ed25519 should pass"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_tampered_digest_returns_401(pool: PgPool) {
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    // 別の body の digest を指定 → digest mismatch
    let wrong_digest = digest::format_cavage(b"different body");
    let req = build_cavage_post(
        body,
        &priv_pem,
        &keyid,
        &now_http_date(),
        Some(&wrong_digest),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_stale_date_returns_401(pool: PgPool) {
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    // ±5 分 clock skew の外 (10 分前)。
    let stale = std::time::SystemTime::now() - std::time::Duration::from_mins(10);
    let req = build_cavage_post(
        body,
        &priv_pem,
        &keyid,
        &httpdate::fmt_http_date(stale),
        None,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_unknown_keyid_returns_401(pool: PgPool) {
    let (priv_pem, _pub_pem) = fresh_rsa();
    // remote actor を **insert しない**。
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/ghost#main-key");
    let req = build_cavage_post(body, &priv_pem, &keyid, &now_http_date(), None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn missing_signature_header_returns_400(pool: PgPool) {
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let resp = app
        .oneshot(
            Request::post("/inbox")
                .header("content-type", "application/activity+json")
                .body(Body::from(r#"{"type":"Create"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rsa_signature_with_ed25519_keyid_is_rejected(pool: PgPool) {
    let (rsa_priv, rsa_pub) = fresh_rsa();
    let (_ed_priv, ed_pub) = fresh_ed25519();
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    // cavage 経路で `#ed25519-key` keyId を主張する不整合。
    // sign module は keyKind を fragment から判定し、cavage の場合は
    // KeyKind::Ed25519 を拒否する (UnsupportedKeyKind → 401)。
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#ed25519-key");
    let req = build_cavage_post(body, &rsa_priv, &keyid, &now_http_date(), None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn user_inbox_path_also_verifies_signature(pool: PgPool) {
    // shared inbox だけでなく `/users/<name>/inbox` でも検証経路が同一であることを確認。
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    // local actor (受信先) も必要。
    let local = NewActor {
        ap_id: format!("https://{HOST}/users/alice"),
        preferred_username: "alice".into(),
        host: HOST.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("https://{HOST}/users/alice/inbox"),
        shared_inbox_url: Some(format!("https://{HOST}/inbox")),
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: format!("https://{HOST}/users/alice#main-key"),
        public_key_pem: pub_pem.clone(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
    };
    repo::actor::insert(&pool, local).await.unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    let path = "/users/alice/inbox";
    let date = now_http_date();
    let digest_value = digest::format_cavage(body);
    let req_partial = Request::post(path)
        .header("host", HOST)
        .header("date", &date)
        .header("digest", &digest_value)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let req_headers = req_partial.headers().clone();
    let covered = ["(request-target)", "host", "date", "digest"];
    let base =
        cavage::build_signature_base("POST", path, &covered, &req_headers, None, None).unwrap();
    let sig_b64 = B64.encode(cavage::sign_rsa_sha256(base.as_bytes(), &priv_pem).unwrap());
    let sig_header = format!(
        "keyId=\"{keyid}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date digest\",signature=\"{sig_b64}\""
    );
    let mut req = req_partial;
    req.headers_mut().insert(
        HeaderName::from_static("signature"),
        HeaderValue::from_str(&sig_header).unwrap(),
    );

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

// ===========================================================================
// Regression tests for PR #19 review findings (F1, F2, F4, F5, F7, F8).
// ===========================================================================

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_missing_digest_in_covered_returns_400(pool: PgPool) {
    // F2: digest が covered に無いと MITM がボディ+Digest を差し替えて
    // 通せる。最小 covered set 強制で 400 で弾く。
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    let req = build_cavage_post_with_covered(
        body,
        &priv_pem,
        &keyid,
        &now_http_date(),
        None,
        &["(request-target)", "host", "date"], // digest を抜く
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_missing_host_in_covered_returns_400(pool: PgPool) {
    // F4: host が covered に無いと別宛先への replay が可能。
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    let req = build_cavage_post_with_covered(
        body,
        &priv_pem,
        &keyid,
        &now_http_date(),
        None,
        &["(request-target)", "date", "digest"], // host を抜く
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_missing_content_digest_in_covered_returns_400(pool: PgPool) {
    // F1: content-digest が covered に無いとボディが一切検証されない。
    let (rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();
    let _ = rsa_priv;
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#ed25519-key");
    let req = build_rfc9421_post_with_covered(
        body,
        &ed_priv,
        &keyid,
        &now_http_date(),
        &["@method", "@target-uri", "host", "date"], // content-digest を抜く
        None,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_empty_covered_returns_400(pool: PgPool) {
    // F1 + F4 同時: `sig1=();...` の空 covered は何もコミットしないので
    // 拒否する。
    let (rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();
    let _ = rsa_priv;
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#ed25519-key");
    let req = build_rfc9421_post_with_covered(body, &ed_priv, &keyid, &now_http_date(), &[], None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_created_overflow_does_not_panic(pool: PgPool) {
    // F5: created=i64::MAX で SystemTime + Duration がパニックしないこと。
    // 認証突破せず inbox を落とせるとマズい。401 / 400 のいずれかが返れば OK。
    let (rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();
    let _ = rsa_priv;
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#ed25519-key");
    let req = build_rfc9421_post_with_covered(
        body,
        &ed_priv,
        &keyid,
        &now_http_date(),
        &["@method", "@target-uri", "host", "date", "content-digest"],
        Some(i64::MAX),
    );
    let resp = app.oneshot(req).await.unwrap();
    // どちらに分類されるかは実装詳細だが、サーバが panic していない (= 5xx
    // でも 4xx でも何か返している) ことだけは厳密に保証する。
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::BAD_REQUEST,
        "created=i64::MAX should be rejected, got {}",
        resp.status()
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_iso8601_date_returns_400_not_401(pool: PgPool) {
    // F7: ISO 8601 形式 (RFC 7231 非準拠) の Date は構造的不備として 400。
    // 401 だと Mastodon 系の再送ループが止まらない。
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let body = br#"{"type":"Follow"}"#;
    let keyid = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}#main-key");
    let req = build_cavage_post(body, &priv_pem, &keyid, "2024-01-01T00:00:00Z", None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_unterminated_quoted_keyid_returns_400(pool: PgPool) {
    // F8: keyId="..." の閉じ忘れは無言で受理されず、Malformed → 400 で弾く。
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let bad_sig =
        "keyId=\"https://example/users/a#main-key,algorithm=\"rsa-sha256\",signature=\"AAAA\"";
    let resp = app
        .oneshot(
            Request::post("/inbox")
                .header("host", HOST)
                .header("date", now_http_date())
                .header("content-type", "application/activity+json")
                .header("signature", bad_sig)
                .body(Body::from(r#"{"type":"Follow"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
