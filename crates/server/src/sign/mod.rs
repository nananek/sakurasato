//! `ActivityPub` HTTP 署名の生成と検証。
//!
//! 受信側は **cavage HTTP signatures draft-12** (Mastodon / Misskey / 旧
//! `Fedibird` など) と **RFC 9421 HTTP Message Signatures + Ed25519**
//! (`Nekonoverse` / Mitra / `GoToSocial` など FEP-521a Multikey を公開する
//! 実装) の両系統をサポートする。送信側は M3b-2 では cavage RSA-SHA256
//! のみ (Mastodon 系で受理される最大公約数)。Ed25519 送出は M3b-3 以降。
//!
//! 公開エントリ:
//! - [`extract_signature_infos`] : 鍵 lookup 前の段階。スキーム判定と
//!   keyId 抽出のみ。DB アクセスせず純粋に headers だけ見る。RFC 9421 の
//!   複数ラベルは順序を保って全件返す
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
pub(crate) mod sign_request;

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
///
/// RFC 9421 の複数ラベル送信に対応するため、ラベル名を保持する。cavage は
/// 構造的に単一署名なので `label: None`。
#[derive(Debug, Clone)]
pub(crate) struct SignatureInfo {
    pub scheme: SigScheme,
    pub key_id: String,
    pub key_kind: KeyKind,
    /// RFC 9421 ラベル名 (`sig1` 等)。cavage では使わない (`None`)。
    pub label: Option<String>,
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

    // --- 503 Service Unavailable: サーバ側の一時障害 (DB 接続失敗等)。
    //     Mastodon は 5xx を長めに保持してリトライするので、DB が回復すれば
    //     アクティビティを失わない。401 (UnknownActor) で握ると相手側キュー
    //     から早期に破棄される可能性がある。
    #[error("internal server error during signature verification")]
    Internal,
}

#[derive(Debug, Clone, Copy)]
enum SigErrorClass {
    BadRequest,
    Unauthorized,
    Internal,
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
            Self::Internal => SigErrorClass::Internal,
        }
    }
}

impl IntoResponse for SigError {
    fn into_response(self) -> Response {
        // 検証失敗の中身はレスポンスに出さない (情報漏洩防止)。
        // ログには出すが、秘密鍵や signature 本体は元々持っていないので安全。
        match self.class() {
            SigErrorClass::Internal => {
                tracing::error!(error = %self, "inbox internal error");
                (StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response()
            }
            SigErrorClass::BadRequest => {
                tracing::warn!(error = %self, "inbox signature verification failed");
                (StatusCode::BAD_REQUEST, "bad request").into_response()
            }
            SigErrorClass::Unauthorized => {
                tracing::warn!(error = %self, "inbox signature verification failed");
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
        }
    }
}

/// 受信ヘッダから署名スキームを判定し、keyId と種別を取り出す。複数ラベル
/// の RFC 9421 ヘッダなら **すべて**のラベルを返す (順序保持)。
///
/// **DB アクセスせず**、純粋に HTTP ヘッダだけを見る。スキーム判定は
/// `Signature-Input` の有無で行う (Mastodon と Nekonoverse が併用する
/// 場合でも、Nekonoverse 系は必ず `Signature-Input` を送る運用)。
///
/// 複数ラベル時は呼び出し側 (`extract.rs`) が順に actor lookup + 検証を
/// 試み、いずれか一つが成立した時点で受理する。
pub(crate) fn extract_signature_infos(headers: &HeaderMap) -> Result<Vec<SignatureInfo>, SigError> {
    if headers.contains_key("signature-input") {
        let input_raw = header_value(headers, "signature-input")?;
        let parsed_all = rfc9421::parse_signature_input_dict(input_raw)
            .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;
        let mut out = Vec::with_capacity(parsed_all.len());
        for parsed in parsed_all {
            let key_id = parsed
                .keyid
                .ok_or_else(|| SigError::SignatureMalformed("missing keyid parameter".into()))?
                .to_string();
            let kind = classify_key_id(&key_id)?;
            out.push(SignatureInfo {
                scheme: SigScheme::Rfc9421,
                key_id,
                key_kind: kind,
                label: Some(parsed.label.to_string()),
            });
        }
        Ok(out)
    } else if headers.contains_key("signature") {
        let sig_raw = header_value(headers, "signature")?;
        let parsed = cavage::parse_signature_header(sig_raw)
            .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;
        let key_id = parsed.key_id.to_string();
        let kind = classify_key_id(&key_id)?;
        Ok(vec![SignatureInfo {
            scheme: SigScheme::Cavage,
            key_id,
            key_kind: kind,
            label: None,
        }])
    } else {
        Err(SigError::SignatureMissing)
    }
}

/// 単一ラベル前提の旧 API。複数ラベルが届いても最初のラベルだけを返す。
/// 新規コードは [`extract_signature_infos`] を使うこと。
#[cfg(test)]
pub(crate) fn extract_signature_info(headers: &HeaderMap) -> Result<SignatureInfo, SigError> {
    let mut infos = extract_signature_infos(headers)?;
    Ok(infos.remove(0))
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
    verify_request_with_actor_at(ctx, info, actor, SystemTime::now)
}

/// テスト向け: 現在時刻を注入できる版。`Fn() -> SystemTime` を取るので
/// 固定値を返すクロージャを `move` で食わせて検証時刻を凍結できる。
pub(crate) fn verify_request_with_actor_at<F: Fn() -> SystemTime>(
    ctx: &RequestContext<'_>,
    info: &SignatureInfo,
    actor: &ActorRow,
    now: F,
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

    // **最小 covered set 強制** (F1+F2+F4): POST inbox では署名が
    // `(request-target)+host+date+digest` の 4 要素すべてにコミットしている
    // ことを必須化する。digest が covered に無いと MITM がボディ+Digest
    // ヘッダを差し替えても署名検証が通ってしまう (= ボディ完全性が崩れる)。
    // host が無いと別宛先への replay が成立、date が無いと clock skew 制約が
    // 名目化、(request-target) が無いと別エンドポイントへの転送が通る。
    require_covered_cavage(&parsed.headers)?;

    // Date のずれを確認 (clock skew)。Date の **パース失敗** は構造的
    // 不備として 400 で返す (F7): ISO 8601 等 RFC 7231 非準拠の値を 401 に
    // すると Mastodon 系が無限リトライするため。
    let date_str = header_value(ctx.headers, "date").map_err(|_| SigError::DateMissing)?;
    let date_time = httpdate::parse_http_date(date_str)
        .map_err(|e| SigError::SignatureMalformed(format!("Date header is not RFC 7231: {e}")))?;
    check_skew(date_time, now)?;

    // Digest 検証。covered 強制で `digest` 必須化済みなので、ヘッダ存在 +
    // 内容一致を独立に確認 (これで body 改竄を弾く)。
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
    // 複数ラベルに対応するため dict 全体をパースし、info.label と一致する
    // エントリを取り出す。`info.label` が `None` の場合は単一ラベル前提で
    // 最初を選ぶ (テスト fixture などで明示的にラベルを持たない構築をした
    // ケース)。
    let parsed_all = rfc9421::parse_signature_input_dict(input_header)
        .map_err(|e| SigError::SignatureMalformed(e.to_string()))?;
    let parsed = match info.label.as_deref() {
        Some(target) => parsed_all
            .into_iter()
            .find(|p| p.label == target)
            .ok_or_else(|| {
                SigError::SignatureMalformed(format!("label {target:?} not found in dict"))
            })?,
        None => parsed_all
            .into_iter()
            .next()
            .ok_or_else(|| SigError::SignatureMalformed("Signature-Input dict is empty".into()))?,
    };

    // **最小 covered set 強制** (F1+F4): POST inbox では署名が `@method` /
    // `@target-uri` / (`host` か `@authority`) / `content-digest` のすべてに
    // コミットしている必要がある。空 covered (`sig1=();...`) や
    // `content-digest` を省いた署名は、ボディ・宛先・メソッドを保護しない。
    require_covered_rfc9421(&parsed.covered)?;

    // alg が明記されていれば key_kind との整合を確認。
    if let Some(alg) = parsed.alg {
        match (info.key_kind, alg) {
            (KeyKind::Ed25519, "ed25519") | (KeyKind::Rsa, "rsa-v1_5-sha256") => {}
            _ => return Err(SigError::AlgMismatch),
        }
    }

    // clock skew: created があれば使う、無ければ Date ヘッダで代替。
    let event_time = if let Some(created) = parsed.created {
        let secs = u64::try_from(created).map_err(|_| SigError::ClockSkew)?;
        // `SystemTime::UNIX_EPOCH + Duration::from_secs(u64::MAX)` は内部の
        // timespec オーバーフローでパニックする実装が存在する (F5)。
        // checked_add で安全に弾く。
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(secs))
            .ok_or(SigError::ClockSkew)?
    } else {
        let date_str = header_value(ctx.headers, "date").map_err(|_| SigError::DateMissing)?;
        httpdate::parse_http_date(date_str).map_err(|e| {
            SigError::SignatureMalformed(format!("Date header is not RFC 7231: {e}"))
        })?
    };
    check_skew(event_time, now)?;

    // Content-Digest 検証。covered 強制で必須化済みなので、ヘッダ存在 +
    // 内容一致を独立に確認。
    let header = ctx
        .headers
        .get("content-digest")
        .and_then(|v| v.to_str().ok());
    digest::verify_content_digest(ctx.body, header).map_err(|e| map_digest_err(&e))?;

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

/// cavage POST inbox に必須な最小 covered headers。1 つでも欠けると
/// ボディ完全性 / 宛先 / 時刻 / メソッドのどれかが署名にコミットされない。
const CAVAGE_REQUIRED_COVERED: &[&str] = &["(request-target)", "host", "date", "digest"];

fn require_covered_cavage(headers: &[&str]) -> Result<(), SigError> {
    for needed in CAVAGE_REQUIRED_COVERED {
        if !headers.contains(needed) {
            return Err(SigError::SignatureMalformed(format!(
                "cavage covered headers must include {needed}"
            )));
        }
    }
    Ok(())
}

/// RFC 9421 POST inbox に必須な最小 covered components。`host` は
/// `@authority` (derived component) で代替できる。
fn require_covered_rfc9421(covered: &[&str]) -> Result<(), SigError> {
    for needed in ["@method", "@target-uri", "content-digest"] {
        if !covered.contains(&needed) {
            return Err(SigError::SignatureMalformed(format!(
                "RFC 9421 covered must include {needed}"
            )));
        }
    }
    if !covered.contains(&"host") && !covered.contains(&"@authority") {
        return Err(SigError::SignatureMalformed(
            "RFC 9421 covered must include host or @authority".to_string(),
        ));
    }
    Ok(())
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

    #[test]
    fn internal_error_returns_503() {
        // F6: DB エラーなど内部障害は 503 を返し、相手のリトライ保持を
        // 長く取らせる (401 だと早期に破棄される)。
        let body = SigError::Internal.into_response();
        assert_eq!(body.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn require_covered_cavage_accepts_full_set() {
        require_covered_cavage(&["(request-target)", "host", "date", "digest"]).unwrap();
    }

    #[test]
    fn require_covered_cavage_accepts_extra_headers() {
        // 必須要素を含んだ上で extra なヘッダがあっても OK。
        require_covered_cavage(&[
            "(request-target)",
            "host",
            "date",
            "digest",
            "content-type",
            "user-agent",
        ])
        .unwrap();
    }

    #[test]
    fn require_covered_cavage_rejects_missing_digest() {
        // F2: digest が covered に無いと MITM がボディ差し替え可能。
        let err = require_covered_cavage(&["(request-target)", "host", "date"]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }

    #[test]
    fn require_covered_cavage_rejects_missing_host() {
        // F4: host が covered に無いと別宛先への replay が可能。
        let err = require_covered_cavage(&["(request-target)", "date", "digest"]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }

    #[test]
    fn require_covered_cavage_rejects_empty() {
        let err = require_covered_cavage(&[]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }

    #[test]
    fn require_covered_rfc9421_accepts_canonical_set() {
        require_covered_rfc9421(&["@method", "@target-uri", "host", "date", "content-digest"])
            .unwrap();
    }

    #[test]
    fn require_covered_rfc9421_accepts_authority_instead_of_host() {
        // host の代わりに @authority (derived component) でも OK。
        require_covered_rfc9421(&["@method", "@target-uri", "@authority", "content-digest"])
            .unwrap();
    }

    #[test]
    fn require_covered_rfc9421_rejects_missing_content_digest() {
        // F1: content-digest が無いとボディ完全性が崩れる。
        let err = require_covered_rfc9421(&["@method", "@target-uri", "host"]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }

    #[test]
    fn require_covered_rfc9421_rejects_empty() {
        // 空 covered (`sig1=();...`) は受理しない。
        let err = require_covered_rfc9421(&[]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }

    #[test]
    fn require_covered_rfc9421_rejects_missing_host_and_authority() {
        // F4: host も @authority も無ければ宛先 binding が無い。
        let err =
            require_covered_rfc9421(&["@method", "@target-uri", "content-digest"]).unwrap_err();
        assert!(matches!(err, SigError::SignatureMalformed(_)));
    }
}
