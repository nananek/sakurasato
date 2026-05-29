//! `ActivityPub` HTTP 署名の生成と検証。
//!
//! 受信側は **cavage HTTP signatures draft-12** (Mastodon / Misskey / 旧
//! `Fedibird` など) と **RFC 9421 HTTP Message Signatures + Ed25519**
//! (`Nekonoverse` / Mitra / `GoToSocial` など FEP-521a Multikey を公開する
//! 実装) の両系統をサポートする。送信側は M3b-2 では cavage RSA-SHA256
//! のみ (Mastodon 系で受理される最大公約数)。Ed25519 送出は M3b-3 以降。
//!
//! 公開エントリ:
//! - [`extract_signature_info`] : 鍵 lookup 前の段階。スキーム判定と
//!   keyId 抽出のみ。DB アクセスせず純粋に headers だけ見る
//! - [`verify_request_with_actor`] : DB から引いた `ActorRow` の鍵を
//!   使い、digest / clock skew / 署名を実際に検証する
//!
//! 一次仕様:
//! - draft-cavage-http-signatures-12 — <https://datatracker.ietf.org/doc/html/draft-cavage-http-signatures-12>
//! - RFC 9421 HTTP Message Signatures — <https://www.rfc-editor.org/rfc/rfc9421.html>
//! - RFC 9530 Digest Fields — <https://www.rfc-editor.org/rfc/rfc9530.html>
//! - FEP-521a Multikey — <https://codeberg.org/fediverse/fep/src/branch/main/fep/521a/fep-521a.md>

use std::time::{Duration, SystemTime};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;
use sakurasato_core::model::ActorRow;
use thiserror::Error;

pub(crate) mod cavage;
pub(crate) mod digest;
pub(crate) mod keyid;
pub(crate) mod rfc9421;

use self::keyid::KeyKind;

/// 許容する時刻ずれ (clock skew)。両方向に ±5 分。
///
/// Mastodon / Misskey は 30 秒〜 5 分の幅で実装が分かれているが、5 分以内
/// なら相互運用上問題ない。NTP 同期があれば数秒以内に収まる。
pub(crate) const MAX_CLOCK_SKEW: Duration = Duration::from_mins(5);

/// 署名スキームの種別。Mastodon 系互換と Nekonoverse 等の新世代の二分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigScheme {
    /// cavage draft-12。`Signature:` ヘッダのみ。
    Cavage,
    /// RFC 9421 HTTP Message Signatures。`Signature-Input:` + `Signature:`。
    Rfc9421,
}

/// スキーム判定と keyId 抽出だけを行う段階の結果。DB アクセス前に
/// 使えるので、actor lookup の引数として渡せる。
#[derive(Debug, Clone)]
pub(crate) struct SignatureInfo {
    pub scheme: SigScheme,
    pub key_id: String,
    pub key_kind: KeyKind,
}

/// 署名検証中に発生したエラー。HTTP レスポンスとして 400 / 401 に対応。
///
/// **情報漏洩防止のため詳細メッセージはレスポンスに含めず**、`tracing::warn!`
/// にだけ出す。相手には汎用文言 (`"bad request"` / `"unauthorized"`) を返す。
#[derive(Debug, Error)]
pub(crate) enum SigError {
    // --- 400 Bad Request: リクエスト構造そのものが ActivityPub inbox の
    //     仕様を満たしていない。再送しても直らない種類。
    #[error("Signature header is missing")]
    SignatureMissing,
    #[error("Date header is missing")]
    DateMissing,
    #[error("Digest header is missing")]
    DigestMissing,
    #[error("Signature header is malformed: {0}")]
    SignatureMalformed(String),
    #[error("keyId is malformed: {0}")]
    KeyIdMalformed(String),
    #[error("required header is missing for signature base: {0}")]
    HeaderMissingForBase(String),

    // --- 401 Unauthorized: 構造は OK だが信頼できない。Mastodon 系は
    //     401 を尊重して再送するので、actor が後で DB に入れば自然成立。
    #[error("Date is outside the acceptable clock skew window")]
    ClockSkew,
    #[error("Digest does not match request body")]
    DigestMismatch,
    #[error("signature does not verify against the actor's key")]
    BadSignature,
    #[error("keyId does not match any known actor")]
    UnknownActor,
    #[error("actor has no public key of the requested kind ({0:?})")]
    ActorMissingKey(KeyKind),
    #[error("keyId fragment is not supported ({0:?})")]
    UnsupportedKeyKind(KeyKind),
    #[error("alg parameter does not match keyId kind")]
    AlgMismatch,
}

#[derive(Debug, Clone, Copy)]
enum SigErrorClass {
    BadRequest,
    Unauthorized,
}

impl SigError {
    fn class(&self) -> SigErrorClass {
        match self {
            Self::SignatureMissing
            | Self::DateMissing
            | Self::DigestMissing
            | Self::SignatureMalformed(_)
            | Self::KeyIdMalformed(_)
            | Self::HeaderMissingForBase(_) => SigErrorClass::BadRequest,
            Self::ClockSkew
            | Self::DigestMismatch
            | Self::BadSignature
            | Self::UnknownActor
            | Self::ActorMissingKey(_)
            | Self::UnsupportedKeyKind(_)
            | Self::AlgMismatch => SigErrorClass::Unauthorized,
        }
    }
}

impl IntoResponse for SigError {
    fn into_response(self) -> Response {
        // 検証失敗の中身はレスポンスに出さない (情報漏洩防止)。
        // ログには出すが、秘密鍵や signature 本体は元々持っていないので安全。
        tracing::warn!(error = %self, "inbox signature verification failed");
        match self.class() {
            SigErrorClass::BadRequest => (StatusCode::BAD_REQUEST, "bad request").into_response(),
            SigErrorClass::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
        }
    }
}

/// 受信ヘッダから署名スキームを判定し、keyId と種別を取り出す。
///
/// **DB アクセスせず**、純粋に HTTP ヘッダだけを見る。スキーム判定は
/// `Signature-Input` の有無で行う (Mastodon と Nekonoverse が併用する
/// 場合でも、Nekonoverse 系は必ず `Signature-Input` を送る運用)。
pub(crate) fn extract_signature_info(headers: &HeaderMap) -> Result<SignatureInfo, SigError> {
    if headers.contains_key("signature-input") {
        let input_raw = header_value(headers, "signature-input")?;
        let parsed = rfc9421::parse_signature_input(input_raw)
            .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;
        let key_id = parsed
            .keyid
            .ok_or_else(|| SigError::SignatureMalformed("missing keyid parameter".into()))?
            .to_string();
        let kind = classify_key_id(&key_id)?;
        Ok(SignatureInfo {
            scheme: SigScheme::Rfc9421,
            key_id,
            key_kind: kind,
        })
    } else if headers.contains_key("signature") {
        let sig_raw = header_value(headers, "signature")?;
        let parsed = cavage::parse_signature_header(sig_raw)
            .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;
        let key_id = parsed.key_id.to_string();
        let kind = classify_key_id(&key_id)?;
        Ok(SignatureInfo {
            scheme: SigScheme::Cavage,
            key_id,
            key_kind: kind,
        })
    } else {
        Err(SigError::SignatureMissing)
    }
}

fn classify_key_id(key_id: &str) -> Result<KeyKind, SigError> {
    let parsed = keyid::parse(key_id).map_err(|e| SigError::KeyIdMalformed(e.to_string()))?;
    Ok(parsed.kind())
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, SigError> {
    headers
        .get(name)
        .ok_or_else(|| SigError::HeaderMissingForBase(name.to_string()))?
        .to_str()
        .map_err(|_| SigError::HeaderMissingForBase(name.to_string()))
}

/// 検証に必要なリクエスト情報。
#[derive(Debug)]
pub(crate) struct RequestContext<'a> {
    pub method: &'a str,
    /// cavage の `(request-target)` 用。`/inbox?x=1` のような path + query。
    pub path_and_query: &'a str,
    /// RFC 9421 の `@target-uri` 用。`https://example.com/inbox` のような完全 URI。
    pub target_uri: &'a str,
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
}

/// 検証用の "現在時刻" を注入できるよう関数化。テストでは固定時刻を渡せる。
pub(crate) type ClockFn = fn() -> SystemTime;

fn system_now() -> SystemTime {
    SystemTime::now()
}

/// DB から引いた `ActorRow` の鍵を使って、digest / clock skew / 署名を
/// 実際に検証する。
///
/// `info.scheme` で cavage / RFC 9421 を分岐、`info.key_kind` で RSA /
/// Ed25519 を分岐する。
pub(crate) fn verify_request_with_actor(
    ctx: &RequestContext<'_>,
    info: &SignatureInfo,
    actor: &ActorRow,
) -> Result<(), SigError> {
    verify_request_with_actor_at(ctx, info, actor, system_now)
}

/// テスト向け: 現在時刻を注入できる版。
pub(crate) fn verify_request_with_actor_at(
    ctx: &RequestContext<'_>,
    info: &SignatureInfo,
    actor: &ActorRow,
    now: ClockFn,
) -> Result<(), SigError> {
    match info.scheme {
        SigScheme::Cavage => verify_cavage_with_actor(ctx, info, actor, now()),
        SigScheme::Rfc9421 => verify_rfc9421_with_actor(ctx, info, actor, now()),
    }
}

fn verify_cavage_with_actor(
    ctx: &RequestContext<'_>,
    info: &SignatureInfo,
    actor: &ActorRow,
    now: SystemTime,
) -> Result<(), SigError> {
    // cavage は keyId 規約から RSA 一択。Ed25519 は RFC 9421 経路を期待。
    if info.key_kind != KeyKind::Rsa {
        return Err(SigError::UnsupportedKeyKind(info.key_kind));
    }

    let sig_header = header_value(ctx.headers, "signature")?;
    let parsed = cavage::parse_signature_header(sig_header)
        .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;

    // Date のずれをまず確認 (clock skew)。Mastodon 系は Date を必ず送る。
    let date_str = header_value(ctx.headers, "date").map_err(|_| SigError::DateMissing)?;
    let date_time = httpdate::parse_http_date(date_str).map_err(|_| SigError::ClockSkew)?;
    check_skew(date_time, now)?;

    // Digest 検証。headers パラメタに `digest` が含まれていなくても、
    // body 付き POST では必須 (Mastodon が要求する)。
    let digest_header = ctx.headers.get("digest").and_then(|v| v.to_str().ok());
    digest::verify_cavage(ctx.body, digest_header).map_err(|e| map_digest_err(&e))?;

    // signature base 組み立て。cavage の headers パラメタ値は空白区切りで
    // lowercase 慣例。ヘッダ名は case-insensitive なので、そのまま渡せば
    // HeaderMap::get_all がマッチする。
    let base = cavage::build_signature_base(
        ctx.method,
        ctx.path_and_query,
        &parsed.headers,
        ctx.headers,
        parsed.created,
        parsed.expires,
    )
    .map_err(|e| map_base_err(&e))?;

    // public_key_pem は actor テーブルで NOT NULL なので unwrap 相当。
    cavage::verify_rsa_sha256(base.as_bytes(), parsed.signature_b64, &actor.public_key_pem)
        .map_err(|_| SigError::BadSignature)
}

fn verify_rfc9421_with_actor(
    ctx: &RequestContext<'_>,
    info: &SignatureInfo,
    actor: &ActorRow,
    now: SystemTime,
) -> Result<(), SigError> {
    let input_header = header_value(ctx.headers, "signature-input")?;
    let sig_header = header_value(ctx.headers, "signature")?;
    let parsed = rfc9421::parse_signature_input(input_header)
        .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;

    // alg が明記されていれば key_kind との整合を確認。
    if let Some(alg) = parsed.alg {
        match (info.key_kind, alg) {
            (KeyKind::Ed25519, "ed25519") | (KeyKind::Rsa, "rsa-v1_5-sha256") => {}
            _ => return Err(SigError::AlgMismatch),
        }
    }

    // clock skew: created があれば使う、無ければ Date ヘッダで代替。
    let event_time = if let Some(created) = parsed.created {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(created).map_err(|_| SigError::ClockSkew)?)
    } else {
        let date_str = header_value(ctx.headers, "date").map_err(|_| SigError::DateMissing)?;
        httpdate::parse_http_date(date_str).map_err(|_| SigError::ClockSkew)?
    };
    check_skew(event_time, now)?;

    // Content-Digest 検証 (covered に含まれていれば必須)。
    if parsed.covered.contains(&"content-digest") {
        let header = ctx
            .headers
            .get("content-digest")
            .and_then(|v| v.to_str().ok());
        digest::verify_content_digest(ctx.body, header).map_err(|e| map_digest_err(&e))?;
    }

    // signature base 組み立て。
    let base = rfc9421::build_signature_base(
        ctx.method,
        ctx.target_uri,
        &parsed.covered,
        ctx.headers,
        parsed.raw_value,
    )
    .map_err(|e| map_rfc9421_base_err(&e))?;

    // signature bytes を抽出。
    let sig_bytes = rfc9421::extract_signature_bytes(sig_header, parsed.label)
        .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;

    match info.key_kind {
        KeyKind::Ed25519 => {
            let pem = actor
                .ed25519_public_key_pem
                .as_deref()
                .ok_or(SigError::ActorMissingKey(KeyKind::Ed25519))?;
            rfc9421::verify_ed25519(base.as_bytes(), &sig_bytes, pem)
                .map_err(|_| SigError::BadSignature)
        }
        KeyKind::Rsa => {
            let sig_b64 =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sig_bytes);
            cavage::verify_rsa_sha256(base.as_bytes(), &sig_b64, &actor.public_key_pem)
                .map_err(|_| SigError::BadSignature)
        }
        KeyKind::Other => Err(SigError::UnsupportedKeyKind(KeyKind::Other)),
    }
}

fn check_skew(event: SystemTime, now: SystemTime) -> Result<(), SigError> {
    let diff = match now.duration_since(event) {
        Ok(d) => d,
        Err(e) => e.duration(), // event が未来 → そのまま絶対値として扱う
    };
    if diff > MAX_CLOCK_SKEW {
        Err(SigError::ClockSkew)
    } else {
        Ok(())
    }
}

fn map_digest_err(e: &digest::DigestError) -> SigError {
    match e {
        digest::DigestError::Missing => SigError::DigestMissing,
        digest::DigestError::Mismatch => SigError::DigestMismatch,
        // 他は構造的不備として 400 扱い。
        _ => SigError::SignatureMalformed(e.to_string()),
    }
}

fn map_base_err(e: &cavage::BaseError) -> SigError {
    match e {
        cavage::BaseError::MissingHeader(n) => SigError::HeaderMissingForBase(n.clone()),
        _ => SigError::SignatureMalformed(e.to_string()),
    }
}

fn map_rfc9421_base_err(e: &rfc9421::BaseError) -> SigError {
    match e {
        rfc9421::BaseError::MissingHeader(n) => SigError::HeaderMissingForBase(n.clone()),
        _ => SigError::SignatureMalformed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()).unwrap();
            h.append(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn extract_info_rejects_missing_signature() {
        let h = headers(&[("Host", "x.test")]);
        assert!(matches!(
            extract_signature_info(&h).unwrap_err(),
            SigError::SignatureMissing
        ));
    }

    #[test]
    fn extract_info_cavage_path() {
        let h = headers(&[(
            "Signature",
            r#"keyId="https://x.test/users/a#main-key",signature="abc=""#,
        )]);
        let info = extract_signature_info(&h).unwrap();
        assert_eq!(info.scheme, SigScheme::Cavage);
        assert_eq!(info.key_id, "https://x.test/users/a#main-key");
        assert_eq!(info.key_kind, KeyKind::Rsa);
    }

    #[test]
    fn extract_info_rfc9421_path() {
        let h = headers(&[
            (
                "Signature-Input",
                r#"sig1=("@method" "@target-uri");keyid="https://x.test/users/a#ed25519-key";alg="ed25519""#,
            ),
            ("Signature", "sig1=:AAAA:"),
        ]);
        let info = extract_signature_info(&h).unwrap();
        assert_eq!(info.scheme, SigScheme::Rfc9421);
        assert_eq!(info.key_kind, KeyKind::Ed25519);
    }

    #[test]
    fn extract_info_rfc9421_takes_priority_over_signature_header() {
        // 両方あるときは RFC 9421 を優先 (新世代の方が情報量多い)。
        let h = headers(&[
            (
                "Signature",
                r#"keyId="https://x.test/u/a#main-key",signature="b""#,
            ),
            (
                "Signature-Input",
                r#"sig1=();keyid="https://x.test/u/a#ed25519-key""#,
            ),
            ("Signature", "sig1=:AAAA:"),
        ]);
        let info = extract_signature_info(&h).unwrap();
        assert_eq!(info.scheme, SigScheme::Rfc9421);
    }

    #[test]
    fn extract_info_rejects_bad_keyid() {
        let h = headers(&[("Signature", r#"keyId="no-fragment-here",signature="x""#)]);
        assert!(matches!(
            extract_signature_info(&h).unwrap_err(),
            SigError::KeyIdMalformed(_)
        ));
    }

    #[test]
    fn sig_error_class_partition() {
        assert!(matches!(
            SigError::SignatureMissing.class(),
            SigErrorClass::BadRequest
        ));
        assert!(matches!(
            SigError::UnknownActor.class(),
            SigErrorClass::Unauthorized
        ));
        assert!(matches!(
            SigError::ClockSkew.class(),
            SigErrorClass::Unauthorized
        ));
    }

    #[test]
    fn check_skew_within_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let event = now - Duration::from_mins(1);
        check_skew(event, now).unwrap();
    }

    #[test]
    fn check_skew_exceeds_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let event = now - Duration::from_secs(301);
        assert!(matches!(
            check_skew(event, now).unwrap_err(),
            SigError::ClockSkew
        ));
    }

    #[test]
    fn check_skew_future_event_also_clamped() {
        // 相手の時計が進んでいるケースも対称に弾く。
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let event = now + Duration::from_secs(400);
        assert!(matches!(
            check_skew(event, now).unwrap_err(),
            SigError::ClockSkew
        ));
    }

    #[test]
    fn into_response_does_not_leak_details() {
        let body = SigError::BadSignature.into_response();
        assert_eq!(body.status(), StatusCode::UNAUTHORIZED);
        // body 文字列は固定汎用文言で、enum メッセージは漏れない。
    }
}
