//! アウトバウンド HTTP 署名: 自インスタンスから外部 inbox に POST する
//! `reqwest::Request` に cavage RSA-SHA256 の `Signature` / `Digest` /
//! `Date` / `Host` を載せる。
//!
//! Mastodon / Misskey / `Fedibird` / `Pleroma` 等の主流実装はすべて cavage を
//! 受け入れる (一部は RFC 9421 も受け入れる)。最大公約数として cavage RSA を
//! 採用し、Ed25519 / RFC 9421 での送出は **M3b-3 以降**に回す (相手の
//! `assertionMethod` から Ed25519 公開鍵を持っているか判定する経路が必要で、
//! それは remote actor fetch とセットで実装される)。
//!
//! ## 最小 covered headers
//!
//! `(request-target) host date digest` の 4 つを必ず covered に含める。
//! 受信側 [`crate::sign`] が同じ最小セットを **強制** している (PR1 の F1+F2+F4)
//! ので、ここでも同じセットを送る ── 一方の覆い (cover) が他方の検証より
//! 弱いと、自分の送信が相手で 401 に落ちる。
//!
//! ## body の扱い
//!
//! `reqwest::Request::body()` は `Option<&Body>` を返し、`Body::as_bytes()`
//! が `Some(&[u8])` を返すのは「body を Vec<u8> や &'static [u8] 等の
//! 一括バッファで構築した場合」のみ。配送ワーカは `serde_json::to_vec` で
//! 一括バッファ化してから request を組むので、ここで `as_bytes()` が
//! `None` を返すパスは想定しない。万一 streaming body だった場合は
//! [`SignOutboundError::BodyNotInMemory`] で弾く。

use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use http::HeaderValue;
use http::header::{DATE, HOST};
use sakurasato_core::model::ActorRow;
use thiserror::Error;

use crate::sign::{cavage, digest};

/// アウトバウンド署名のエラー。配送ワーカ側で `anyhow::Error` に
/// 畳まれて `delivery_queue.last_error` に格納される。
#[derive(Debug, Error)]
pub(crate) enum SignOutboundError {
    #[error("local actor has no RSA private key (init must run with valid keypair)")]
    ActorMissingPrivateKey,
    #[error("request URL has no host component")]
    MissingHost,
    #[error("request body is not in-memory; outbound signing requires a buffered body")]
    BodyNotInMemory,
    #[error("RSA signing failed: {0}")]
    Sign(#[from] cavage::SignError),
    #[error("signature base construction failed: {0}")]
    Base(#[from] cavage::BaseError),
    #[error("computed header value contains invalid characters: {0}")]
    InvalidHeaderValue(&'static str),
}

/// covered headers (最小セット)。受信側の検証 [`crate::sign::CAVAGE_REQUIRED_COVERED`]
/// と完全一致させる ── 順序差や欠落は受け手の `400` を招く。
const COVERED_HEADERS: &[&str] = &["(request-target)", "host", "date", "digest"];

/// `req` に cavage RSA-SHA256 署名 + 関連ヘッダを乗せる。
///
/// 呼び出し前提:
/// - `req` の body は in-memory バッファ (`.body(Vec<u8>)` 等で構築)。
/// - `req` の URL は絶対 URL で `host` 部を持つ。
/// - `actor` は **local actor** で、`private_key_pem` を持つ。
///
/// 副作用: `Date` / `Host` / `Digest` / `Signature` ヘッダを `req` に挿入する
/// (既存値があれば上書き)。
pub(crate) fn sign_outbox_request(
    req: &mut reqwest::Request,
    actor: &ActorRow,
) -> Result<(), SignOutboundError> {
    sign_outbox_request_at(req, actor, SystemTime::now())
}

/// テスト向け: 現在時刻を注入できる版。`Date` ヘッダの値が決定的になるので、
/// 既知ベクタとの照合や 既知署名値の roundtrip が書ける。
pub(crate) fn sign_outbox_request_at(
    req: &mut reqwest::Request,
    actor: &ActorRow,
    now: SystemTime,
) -> Result<(), SignOutboundError> {
    let private_pem = actor
        .private_key_pem
        .as_deref()
        .ok_or(SignOutboundError::ActorMissingPrivateKey)?;

    // 1. body bytes を取り出す。streaming body は弾く (BodyNotInMemory)。
    //    `req.body()` が None なのは GET 等で .body(...) 未呼び出しの場合
    //    だけ。POST inbox では呼び出し側が必ず body を載せる契約なので、
    //    None は契約違反として `BodyNotInMemory` で扱う。
    let body_bytes: &[u8] = match req.body() {
        Some(body) => body.as_bytes().ok_or(SignOutboundError::BodyNotInMemory)?,
        None => b"",
    };

    // 2. Host / Date / Digest を決定。URL の host_str + 明示 port から
    //    Host ヘッダ値を組み立てる (デフォルトポート 443/80 は付けない)。
    //    `method` も `headers_mut()` を取る前に確定しておく (借用衝突回避)。
    let method = req.method().as_str().to_string();
    let url = req.url();
    let host_header = build_host_header(url)?;
    let path_and_query = build_path_and_query(url);
    let date_value = httpdate::fmt_http_date(now);
    let digest_value = digest::format_cavage(body_bytes);

    // 3. 計算済みのヘッダ値を request に挿入する。BaseError::MissingHeader
    //    が出ないよう、署名 base 組み立て前に必ず insert しておく。
    let headers = req.headers_mut();
    headers.insert(
        HOST,
        HeaderValue::from_str(&host_header)
            .map_err(|_| SignOutboundError::InvalidHeaderValue("host"))?,
    );
    headers.insert(
        DATE,
        HeaderValue::from_str(&date_value)
            .map_err(|_| SignOutboundError::InvalidHeaderValue("date"))?,
    );
    headers.insert(
        "digest",
        HeaderValue::from_str(&digest_value)
            .map_err(|_| SignOutboundError::InvalidHeaderValue("digest"))?,
    );

    // 4. cavage signature base を組み立て、RSA-SHA256 で署名。
    let base = cavage::build_signature_base(
        &method,
        &path_and_query,
        COVERED_HEADERS,
        headers,
        None,
        None,
    )?;
    let sig_bytes = cavage::sign_rsa_sha256(base.as_bytes(), private_pem)?;
    let sig_b64 = B64.encode(sig_bytes);

    // 5. `Signature:` ヘッダを組み立てる。draft-cavage では各パラメタを
    //    カンマ区切り、文字列値はダブルクォート囲み。`key_id` / `sig_b64`
    //    にダブルクォートが含まれることは規約上ありえない (URI スキーム /
    //    base64 標準アルファベット) が、念のため HeaderValue 経由で
    //    バリデートする。
    let signature_header = format!(
        r#"keyId="{key_id}",algorithm="rsa-sha256",headers="{covered}",signature="{sig_b64}""#,
        key_id = actor.public_key_id,
        covered = COVERED_HEADERS.join(" "),
    );
    headers.insert(
        "signature",
        HeaderValue::from_str(&signature_header)
            .map_err(|_| SignOutboundError::InvalidHeaderValue("signature"))?,
    );

    Ok(())
}

/// HTTP `Host:` ヘッダ値を組み立てる。
///
/// 仕様 (RFC 9110 §7.2): デフォルトポート (https=443 / http=80) は省略、
/// 非デフォルトポートは `host:port` で明示する。reqwest の `url::Url::port()`
/// は **明示指定されたときだけ** `Some` を返す挙動なので、ここで足しても
/// デフォルトポートが入り込むことはない。
fn build_host_header(url: &reqwest::Url) -> Result<String, SignOutboundError> {
    let host = url.host_str().ok_or(SignOutboundError::MissingHost)?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// cavage `(request-target)` 用の `path[?query]` を組み立てる。
///
/// `url::Url::path()` は常に `/` で始まる絶対 path を返し、`query()` は
/// `?` を含まない生クエリ文字列を返す。`fragment` (`#...`) は HTTP
/// リクエストには載らないので考慮しない。
fn build_path_and_query(url: &reqwest::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::{RequestContext, SigScheme, SignatureInfo, keyid::KeyKind};
    use chrono::Utc;
    use http::HeaderMap;
    use reqwest::Client;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::pkcs8::LineEnding;
    use rsa::rand_core::OsRng;
    use sqlx::types::Json;
    use std::time::{Duration, SystemTime};

    fn fresh_actor(ap_id: &str) -> ActorRow {
        let priv_key = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
        let pub_key = priv_key.to_public_key();
        let priv_pem = priv_key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let pub_pem = pub_key.to_public_key_pem(LineEnding::LF).unwrap();
        ActorRow {
            id: 1,
            ap_id: ap_id.to_string(),
            preferred_username: "alice".to_string(),
            host: "x.test".to_string(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: format!("{ap_id}/inbox"),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: format!("{ap_id}#main-key"),
            public_key_pem: pub_pem,
            private_key_pem: Some(priv_pem),
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: Json(vec![]),
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".to_string(),
            manually_approves_followers: false,
            birthday: None,
            location: None,
            lang: None,
            followed_message: None,
            fields: Json(vec![]),
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn build_post_request(url: &str, body: Vec<u8>) -> reqwest::Request {
        Client::new()
            .post(url)
            .header("content-type", "application/activity+json")
            .body(body)
            .build()
            .unwrap()
    }

    #[test]
    fn sign_adds_required_headers() {
        let actor = fresh_actor("https://x.test/users/alice");
        let mut req = build_post_request("https://remote.test/inbox", b"{}".to_vec());
        sign_outbox_request(&mut req, &actor).unwrap();

        let h = req.headers();
        assert!(h.contains_key("date"), "Date must be set");
        assert!(h.contains_key("host"), "Host must be set");
        assert!(h.contains_key("digest"), "Digest must be set");
        assert!(h.contains_key("signature"), "Signature must be set");
    }

    #[test]
    fn signed_request_verifies_with_inbound_path() {
        // 自前で署名 → 自前の検証器でラウンドトリップが通る。これが
        // 通らないと相手で 401 になる (受信側 [`crate::sign`] と最小
        // covered set が一致していない兆候)。
        let actor = fresh_actor("https://x.test/users/alice");
        let body = br#"{"type":"Create"}"#.to_vec();
        let mut req = build_post_request("https://remote.test/inbox", body.clone());
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        sign_outbox_request_at(&mut req, &actor, now).unwrap();

        // 受信側に詰め替え。
        let info = SignatureInfo {
            scheme: SigScheme::Cavage,
            key_id: actor.public_key_id.clone(),
            key_kind: KeyKind::Rsa,
            label: None,
        };
        let headers: HeaderMap = req.headers().clone();
        let ctx = RequestContext {
            method: "POST",
            path_and_query: "/inbox",
            target_uri: "https://remote.test/inbox",
            headers: &headers,
            body: &body,
        };
        // 同じ `now` を流し込めば clock skew で落ちない。
        crate::sign::verify_request_with_actor_at(&ctx, &info, &actor, || now).unwrap();
    }

    #[test]
    fn sign_rejects_actor_without_private_key() {
        let mut actor = fresh_actor("https://x.test/users/alice");
        actor.private_key_pem = None;
        let mut req = build_post_request("https://remote.test/inbox", b"{}".to_vec());
        let err = sign_outbox_request(&mut req, &actor).unwrap_err();
        assert!(matches!(err, SignOutboundError::ActorMissingPrivateKey));
    }

    #[test]
    fn host_header_omits_default_port() {
        // https://remote.test/inbox は Host: remote.test。:443 は付けない。
        let url = reqwest::Url::parse("https://remote.test/inbox").unwrap();
        assert_eq!(build_host_header(&url).unwrap(), "remote.test");
    }

    #[test]
    fn host_header_includes_nondefault_port() {
        // https://remote.test:8443/inbox は Host: remote.test:8443。
        let url = reqwest::Url::parse("https://remote.test:8443/inbox").unwrap();
        assert_eq!(build_host_header(&url).unwrap(), "remote.test:8443");
    }

    #[test]
    fn path_and_query_combines_query_string() {
        let url = reqwest::Url::parse("https://x.test/inbox?foo=bar&baz=qux").unwrap();
        assert_eq!(build_path_and_query(&url), "/inbox?foo=bar&baz=qux");
    }

    #[test]
    fn path_and_query_without_query() {
        let url = reqwest::Url::parse("https://x.test/inbox").unwrap();
        assert_eq!(build_path_and_query(&url), "/inbox");
    }

    #[test]
    fn signature_header_format_is_cavage_compatible() {
        // Mastodon 系がパースできる形式 (key="value" カンマ区切り) で
        // 出ていることを文字列レベルで確認。
        let actor = fresh_actor("https://x.test/users/alice");
        let mut req = build_post_request("https://remote.test/inbox", b"{}".to_vec());
        sign_outbox_request(&mut req, &actor).unwrap();
        let sig = req.headers().get("signature").unwrap().to_str().unwrap();
        assert!(sig.contains(r#"keyId="https://x.test/users/alice#main-key""#));
        assert!(sig.contains(r#"algorithm="rsa-sha256""#));
        assert!(sig.contains(r#"headers="(request-target) host date digest""#));
        assert!(sig.contains(r#"signature=""#));
    }

    #[test]
    fn tampered_body_breaks_signature_verification() {
        // 同じ key で署名 → body を差し替えると digest 不一致または署名
        // 不一致で受信側が拒否することを確認 (MITM 防御の確認)。
        let actor = fresh_actor("https://x.test/users/alice");
        let body = br#"{"type":"Create"}"#.to_vec();
        let tampered = br#"{"type":"Delete"}"#.to_vec();
        let mut req = build_post_request("https://remote.test/inbox", body);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        sign_outbox_request_at(&mut req, &actor, now).unwrap();

        let info = SignatureInfo {
            scheme: SigScheme::Cavage,
            key_id: actor.public_key_id.clone(),
            key_kind: KeyKind::Rsa,
            label: None,
        };
        let headers: HeaderMap = req.headers().clone();
        let ctx = RequestContext {
            method: "POST",
            path_and_query: "/inbox",
            target_uri: "https://remote.test/inbox",
            headers: &headers,
            body: &tampered,
        };
        let err =
            crate::sign::verify_request_with_actor_at(&ctx, &info, &actor, || now).unwrap_err();
        // Digest 検証で落ちる (signature base 検証より前)。
        assert!(matches!(err, crate::sign::SigError::DigestMismatch));
    }
}
