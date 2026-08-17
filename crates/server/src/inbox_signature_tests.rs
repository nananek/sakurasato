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
const LOCAL_USER: &str = "alice";
const REMOTE_HOST: &str = "nekonoverse.test";
const REMOTE_USER: &str = "carol";

fn make_config() -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: HOST.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            public_listen: None,
            local_api_listen: None,
            user: "alice".into(),
            info: sakurasato_core::config::ServerInfo::default(),
            auto_approve_followers_for_followees: false,
            max_note_text_length: 3000,
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
            public_base_url: None,
        },
        media_proxy: sakurasato_core::config::MediaProxyConfig {
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
            video: sakurasato_core::config::VideoConfig::default(),
            emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
        },
        miauth: None,
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
        manually_approves_followers: false,
    }
}

/// テスト用に local actor (受信側 = `{HOST}/users/{LOCAL_USER}`) を組み立てる。
/// `crates/server/tests/dispatch_pg.rs::local_actor` と同じ形 ── 送信側の
/// outbound 署名は M3b-2 時点で cavage RSA-SHA256 のみのため、local actor は
/// RSA 鍵のみ持つ (`priv_pem` はテストでは実際に署名計算に使わないプレース
/// ホルダで良い、`dispatch_pg.rs` と同じ慣習)。
fn local_actor(pub_pem: &str, priv_pem: &str) -> NewActor {
    let ap_id = format!("https://{HOST}/users/{LOCAL_USER}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: LOCAL_USER.into(),
        host: HOST.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{HOST}/inbox")),
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: pub_pem.into(),
        private_key_pem: Some(priv_pem.into()),
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
        manually_approves_followers: false,
    }
}

/// `local_actor` の鍵アカ (`manually_approves_followers = true`) 版。
/// Issue #66 の RFC9421+Ed25519 回帰テスト (下記) で使う。
fn locked_local_actor(pub_pem: &str, priv_pem: &str) -> NewActor {
    let mut a = local_actor(pub_pem, priv_pem);
    a.manually_approves_followers = true;
    a
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

/// cavage signature ヘッダに Ed25519 鍵を載せた POST inbox を組み立てる。
/// nekonoverse 20260524-1 等が送ってくる形 (`#ed25519-key` keyId +
/// `algorithm="ed25519"` + base は cavage と同一規則) を再現する。
fn build_cavage_ed25519_post(
    body: &[u8],
    ed25519_priv_pem: &str,
    keyid: &str,
    date: &str,
) -> Request<Body> {
    let path = "/inbox";
    let digest_value = digest::format_cavage(body);
    let mut req = Request::post(path)
        .header("host", HOST)
        .header("date", date)
        .header("digest", &digest_value)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let covered: &[&str] = &["(request-target)", "host", "date", "digest"];
    let headers = headers_to_map(&req);
    let base = cavage::build_signature_base("POST", path, covered, &headers, None, None).unwrap();
    let sig_bytes = rfc9421::sign_ed25519(base.as_bytes(), ed25519_priv_pem).unwrap();
    let sig_b64 = B64.encode(sig_bytes);
    let headers_param = covered.join(" ");
    let sig_header = format!(
        "keyId=\"{keyid}\",algorithm=\"ed25519\",headers=\"{headers_param}\",signature=\"{sig_b64}\""
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

/// 検証成功 → dispatch でも 202 で受理されるよう、F3 ([`crate::dispatch::verify_body_actor`])
/// を満たす最小 body を組み立てる。
///
/// `type` は本リポジトリで **handler 未実装** の AS2 verb (`View`) を使う ──
/// dispatch の default アームに落ちて handler を経由せず 202 で返るので、
/// local actor / follow / note の seed や `id` / `object` フィールドが不要
/// (= ここでのテストはあくまで「署名検証が通って dispatch 入口に届く」だけ
/// を確認する)。
///
/// 当初は `Announce` を使っていたが、M11 で Announce にも実 handler が
/// 付いたため、`extract_activity_id` の missing-id reject (400) で落ちる
/// ようになった。fallback アームに残るのは `Add` / `Remove` / `Block` /
/// `Flag` / `Read` / `Question` / `View` 等の低頻度 verb。
fn minimal_body_for(signer_ap_id: &str) -> Vec<u8> {
    format!(r#"{{"type":"View","actor":"{signer_ap_id}"}}"#).into_bytes()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_rsa_valid_signature_is_accepted(pool: PgPool) {
    let (priv_pem, pub_pem) = fresh_rsa();
    repo::actor::insert(&pool, build_remote_actor(&pub_pem, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid = format!("{signer}#main-key");
    let req = build_cavage_post(&body, &priv_pem, &keyid, &now_http_date(), None);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "cavage RSA should pass"
    );
}

/// 回帰 ([sakurasato#39](https://github.com/nananek/sakurasato/issues/39)):
/// 実 nekonoverse は cavage 形式の `Signature:` ヘッダに `#ed25519-key` を
/// 載せて POST してくる。sakurasato は cavage → RSA 一択だった時代に
/// `UnsupportedKeyKind(Ed25519)` で 401 を返していたが、cavage + Ed25519 も
/// 検証パスに通す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_ed25519_valid_signature_is_accepted(pool: PgPool) {
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid = format!("{signer}#ed25519-key");
    let req = build_cavage_ed25519_post(&body, &ed_priv, &keyid, &now_http_date());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "cavage with Ed25519 keyId should pass"
    );
}

/// 回帰: actor の Ed25519 鍵が未設定 (RSA only actor) で、それ宛てに
/// cavage Ed25519 が来た場合は 401 (`ActorMissingKey`) を返す ── 「Ed25519
/// 鍵が無いのに ed25519 で署名している」は信頼境界の問題なので、Ed25519
/// 経路を許可しても actor 側に鍵が無ければ依然として拒否しないといけない。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn cavage_ed25519_without_actor_ed_key_is_rejected(pool: PgPool) {
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, _ed_pub) = fresh_ed25519();
    // build_remote_actor で ed25519_public_key_pem は None。
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, None))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid = format!("{signer}#ed25519-key");
    let req = build_cavage_ed25519_post(&body, &ed_priv, &keyid, &now_http_date());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "cavage Ed25519 against RSA-only actor should be 401"
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

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid = format!("{signer}#ed25519-key");
    let req = build_rfc9421_post(&body, &ed_priv, &keyid, &now_http_date());
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
        manually_approves_followers: false,
    };
    repo::actor::insert(&pool, local).await.unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid = format!("{signer}#main-key");
    let path = "/users/alice/inbox";
    let date = now_http_date();
    let digest_value = digest::format_cavage(&body);
    let req_partial = Request::post(path)
        .header("host", HOST)
        .header("date", &date)
        .header("digest", &digest_value)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.clone()))
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

// ---- N1: RFC 9421 複数ラベル ------------------------------------------------
//
// 2 ラベルを併送する送信側を想定。各ラベルは独立した signature base + 鍵で
// 署名され、受信側は OR セマンティクスで「いずれか 1 つが検証成立」したら
// 受理する。実装側は `parse_signature_input_dict` で全ラベルを取り出し、
// `extract.rs::SignedInboxBody::from_request` のループで順に試行する。

/// RFC 9421 で 2 ラベル併送する POST inbox リクエストを組み立てる。
///
/// `(label, keyid, priv_pem, tamper)` ── `tamper=true` のラベルは signature
/// バイトの末尾 1 バイトを反転させ、その鍵での検証だけが落ちる。これにより
/// 「先頭が落ちて 2 番目で受理」「両方落ちて 401」シナリオを 1 つのヘルパで
/// 組める。
fn build_rfc9421_two_label_post(
    body: &[u8],
    date: &str,
    sig1: (&str, &str, &str, bool),
    sig2: (&str, &str, &str, bool),
) -> Request<Body> {
    let (label1, keyid1, priv1, tamper1) = sig1;
    let (label2, keyid2, priv2, tamper2) = sig2;

    let path = "/inbox";
    let target_uri = format!("https://{HOST}{path}");
    let content_digest = digest::format_content_digest(body);
    #[allow(clippy::cast_possible_wrap, reason = "test fixture, secs fits in i64")]
    let created = httpdate::parse_http_date(date)
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut req = Request::post(path)
        .header("host", HOST)
        .header("date", date)
        .header("content-digest", &content_digest)
        .header("content-type", "application/activity+json")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let headers = headers_to_map(&req);

    let covered = ["@method", "@target-uri", "host", "date", "content-digest"];
    let covered_quoted = covered
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(" ");

    let sign_one = |keyid: &str, priv_pem: &str, tamper: bool| -> String {
        let raw_value =
            format!(r#"({covered_quoted});created={created};keyid="{keyid}";alg="ed25519""#);
        let base =
            rfc9421::build_signature_base("POST", &target_uri, &covered, &headers, &raw_value)
                .unwrap();
        let mut sig_bytes = rfc9421::sign_ed25519(base.as_bytes(), priv_pem).unwrap();
        if tamper {
            sig_bytes[63] ^= 0xFF;
        }
        let sig_b64 = B64.encode(sig_bytes);
        format!("RAW={raw_value}|B64={sig_b64}")
    };

    let entry1 = sign_one(keyid1, priv1, tamper1);
    let entry2 = sign_one(keyid2, priv2, tamper2);
    let (raw1, sig1_b64) = entry1.split_once("|B64=").unwrap();
    let (raw2, sig2_b64) = entry2.split_once("|B64=").unwrap();
    let raw1 = raw1.strip_prefix("RAW=").unwrap();
    let raw2 = raw2.strip_prefix("RAW=").unwrap();

    let input_value = format!("{label1}={raw1}, {label2}={raw2}");
    let sig_value = format!("{label1}=:{sig1_b64}:, {label2}=:{sig2_b64}:");
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

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_multi_label_first_valid_is_accepted(pool: PgPool) {
    // sig1 = 既知 actor の正しい署名、sig2 = 不正。先頭で受理されて 202。
    let (rsa_pub, ed_priv, ed_pub) = {
        let (_, rsa_pub) = fresh_rsa();
        let (ed_priv, ed_pub) = fresh_ed25519();
        (rsa_pub, ed_priv, ed_pub)
    };
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid_ed = format!("{signer}#ed25519-key");
    let (other_priv, _other_pub) = fresh_ed25519();
    let req = build_rfc9421_two_label_post(
        &body,
        &now_http_date(),
        ("sig1", &keyid_ed, &ed_priv, false),
        // sig2 は同じ keyId で署名し、tamper で落とす → BadSignature
        ("sig2", &keyid_ed, &other_priv, false),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "multi-label: first valid label should be accepted"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_multi_label_second_valid_when_first_unknown_actor(pool: PgPool) {
    // sig1 = DB に無い keyId、sig2 = 既知 actor の有効署名。
    // 先頭が UnknownActor で落ちても 2 番目で受理されて 202。
    let (rsa_pub, ed_priv, ed_pub) = {
        let (_, rsa_pub) = fresh_rsa();
        let (ed_priv, ed_pub) = fresh_ed25519();
        (rsa_pub, ed_priv, ed_pub)
    };
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid_known = format!("{signer}#ed25519-key");
    let keyid_unknown = "https://other-host.test/users/unknown#ed25519-key".to_string();
    let (other_priv, _other_pub) = fresh_ed25519();
    let req = build_rfc9421_two_label_post(
        &body,
        &now_http_date(),
        ("sig1", &keyid_unknown, &other_priv, false),
        ("sig2", &keyid_known, &ed_priv, false),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "multi-label: should accept when later label verifies"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_multi_label_all_invalid_returns_401(pool: PgPool) {
    // sig1 = 既知 actor だが署名 tamper、sig2 = 別の既知 actor で署名 tamper。
    // どのラベルも検証できなければ 401 (BadSignature)。
    let (rsa_pub, ed_priv, ed_pub) = {
        let (_, rsa_pub) = fresh_rsa();
        let (ed_priv, ed_pub) = fresh_ed25519();
        (rsa_pub, ed_priv, ed_pub)
    };
    repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config());
    let app = router(state);

    let signer = format!("https://{REMOTE_HOST}/users/{REMOTE_USER}");
    let body = minimal_body_for(&signer);
    let keyid_ed = format!("{signer}#ed25519-key");
    let req = build_rfc9421_two_label_post(
        &body,
        &now_http_date(),
        ("sig1", &keyid_ed, &ed_priv, true),
        ("sig2", &keyid_ed, &ed_priv, true),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "multi-label: all-failing should return 401"
    );
}

// ===========================================================================
// RFC 9421 + Ed25519 の Follow/Accept 統合テスト
// (tmp/plan-federation-test-pleroma-mitra-fedibird.md §3/§4)
//
// 背景: 既存の `rfc9421_ed25519_valid_signature_is_accepted` は
// `minimal_body_for` (`type:"View"`, handler 未実装 verb) を使い、署名検証を
// 抜けて dispatch 入口で 202 が返ることしか確認していなかった。「署名検証層
// (cavage/rfc9421) を抜けた後の handler ロジックは署名方式で分岐しない」を
// 実際の Follow activity で検証する。
//
// 配置: `crates/server/tests/dispatch_pg.rs` は crate 外の integration test
// であり、`crate::sign::{cavage, rfc9421}` は `pub(crate)` のため参照できない
// (このファイルの `build_rfc9421_post*` を再実装せず再利用するには、lib 内の
// `#[cfg(test)]` モジュールに置く必要がある)。cavage 版の相当テストは
// dispatch_pg.rs に残したまま、RFC9421+Ed25519 版はここに集約する
// (plan §4 item 3 の「重複セットアップコストが低い方に寄せてよい」判断)。
// ===========================================================================

/// cavage 版 `follow_request_enqueues_accept` (`dispatch_pg.rs`) の
/// RFC9421+Ed25519 移植。新規 Follow → 即 accepted (お一人様 + 自動承認) →
/// Accept が `delivery_queue` に積まれることを、DB 直 assert で検証する
/// (plan §4 の「Follow 実体検証」要件も兼ねる)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_ed25519_follow_is_accepted_and_state_transitions(pool: PgPool) {
    let (local_priv, local_pub) = fresh_rsa();
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, &local_priv))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let follow_id = format!("{}/activities/follow-rfc9421-{}", remote.ap_id, local.id);
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#ed25519-key", remote.ap_id);
    let req = build_rfc9421_post(body.as_bytes(), &ed_priv, &keyid, &now_http_date());

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "RFC9421+Ed25519 Follow must be accepted"
    );

    let follow = sqlx::query!(
        "SELECT id, ap_id, follower_actor_id, followed_actor_id, state FROM follow WHERE ap_id = $1",
        follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(follow.state, "accepted");
    assert_eq!(follow.follower_actor_id, remote.id);
    assert_eq!(follow.followed_actor_id, local.id);

    let queued = sqlx::query!(
        r#"SELECT id, inbox_url, activity as "activity: sqlx::types::Json<serde_json::Value>",
              sender_actor_id, state
          FROM delivery_queue WHERE sender_actor_id = $1 AND state = 'pending'"#,
        local.id,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(queued.len(), 1, "Accept must be queued for delivery");
    let row = &queued[0];
    assert_eq!(row.inbox_url, remote.inbox_url);

    let activity = &row.activity.0;
    assert_eq!(activity["type"], "Accept");
    assert_eq!(activity["actor"], local.ap_id);
    let object = &activity["object"];
    assert_eq!(object["id"], follow_id);
    assert_eq!(object["actor"], remote.ap_id);
    assert_eq!(object["object"], local.ap_id);
}

/// cavage 版 `duplicate_follow_is_idempotent` (`dispatch_pg.rs`) の
/// RFC9421+Ed25519 移植。同じ Follow が二度届いても follow 行は 1 つのまま
/// (accepted に固定、pending へ巻き戻らない) ことを確認する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_ed25519_duplicate_follow_is_idempotent(pool: PgPool) {
    let (local_priv, local_pub) = fresh_rsa();
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, &local_priv))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app1 = router(state.clone());
    let app2 = router(state);

    let follow_id = format!("{}/activities/dupe-rfc9421-{}", remote.ap_id, local.id);
    let body = serde_json::json!({
        "id": follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#ed25519-key", remote.ap_id);

    for app in [app1, app2] {
        let req = build_rfc9421_post(body.as_bytes(), &ed_priv, &keyid, &now_http_date());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    let row = sqlx::query!(
        "SELECT count(*) as c, max(state) as state FROM follow WHERE ap_id = $1",
        follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.c.unwrap_or(0), 1, "follow row must not duplicate");
    assert_eq!(
        row.state.as_deref(),
        Some("accepted"),
        "duplicate Follow must keep state at accepted, not flip back to pending",
    );
}

/// cavage 版 `locked_actor_keeps_inbound_follow_pending` (`dispatch_pg.rs`) の
/// RFC9421+Ed25519 移植。鍵アカ (Issue #66) 宛の Follow は auto-Accept されず
/// `follow.state = pending` で据え置かれ、`delivery_queue` に Accept は
/// 積まれないことを確認する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_ed25519_locked_actor_keeps_inbound_follow_pending(pool: PgPool) {
    let (local_priv, local_pub) = fresh_rsa();
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, &local_priv))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let follow_id = format!(
        "{}/activities/locked-follow-rfc9421-{}",
        remote.ap_id, local.id
    );
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#ed25519-key", remote.ap_id);
    let req = build_rfc9421_post(body.as_bytes(), &ed_priv, &keyid, &now_http_date());

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "locked actor still returns 202 for the inbound Follow (silent hold)",
    );

    let row = sqlx::query!("SELECT state FROM follow WHERE ap_id = $1", follow_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.state, "pending",
        "locked actor must keep follow row at pending until CLI approval",
    );

    let queued = sqlx::query!(
        "SELECT count(*) AS c FROM delivery_queue WHERE sender_actor_id = $1",
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        queued.c.unwrap_or(0),
        0,
        "locked actor must NOT auto-enqueue an Accept activity",
    );

    let inboxes = repo::follow::list_accepted_inboxes(&pool, local.id)
        .await
        .unwrap();
    assert!(
        !inboxes.iter().any(|u| u == &remote.inbox_url),
        "follower of a still-pending Follow must not appear as a delivery target",
    );
}

/// cavage 版 `follow_id_host_mismatch_is_rejected` (`dispatch_pg.rs`) の
/// RFC9421+Ed25519 移植。信頼境界テスト (round-2 F2 回帰) ── 署名検証層を
/// 抜けた後の handler の host 一致チェックが署名方式に依存しないことを確認
/// する (plan §3 item 4)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn rfc9421_ed25519_follow_id_host_mismatch_is_rejected(pool: PgPool) {
    let (local_priv, local_pub) = fresh_rsa();
    let (_rsa_priv, rsa_pub) = fresh_rsa();
    let (ed_priv, ed_pub) = fresh_ed25519();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, &local_priv))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, build_remote_actor(&rsa_pub, Some(&ed_pub)))
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // 署名は remote (Ed25519)、body の actor も remote (F3 は通る)、
    // しかし activity id は good.example のホスト。
    let spoofed_follow_id = "https://good.example/activities/follow-9999-rfc9421";
    let body = serde_json::json!({
        "id": spoofed_follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#ed25519-key", remote.ap_id);
    let req = build_rfc9421_post(body.as_bytes(), &ed_priv, &keyid, &now_http_date());

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_server_error() || resp.status().is_client_error(),
        "spoofed Follow id must be rejected, got {}",
        resp.status(),
    );

    let count = sqlx::query!(
        "SELECT count(*) as c FROM follow WHERE ap_id = $1",
        spoofed_follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count.c.unwrap_or(0),
        0,
        "spoofed Follow id must not be inserted",
    );
}
