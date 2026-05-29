//! RFC 9421 HTTP Message Signatures + Ed25519 のパーサ・signature base 生成・
//! 署名/検証。
//!
//! `Nekonoverse` / Mitra / `GoToSocial` など FEP-521a Multikey で Ed25519
//! 公開鍵を公開する実装は本 RFC の `Signature-Input` / `Signature` を
//! 用いる。
//!
//! 形式 (例):
//! ```text
//! Signature-Input: sig1=("@method" "@target-uri" "host" "date" "content-digest");\
//!                  created=1700000000;keyid="https://example/users/a#ed25519-key";alg="ed25519"
//! Signature: sig1=:<base64>:
//! ```
//!
//! signature base (RFC 9421 §2.5):
//! ```text
//! "@method": POST
//! "@target-uri": https://example.com/inbox
//! "host": example.com
//! "date": Tue, 20 Apr 2021 02:07:56 GMT
//! "content-digest": sha-256=:abc...:
//! "@signature-params": ("@method" "@target-uri" "host" "date" "content-digest");\
//!                      created=1700000000;keyid="...";alg="ed25519"
//! ```
//!
//! M3b-2 では **単一ラベル前提** (Mastodon 系 / Nekonoverse の現実装はすべて
//! 単一ラベル `sig1` 等)。複数ラベルのディスパッチは将来。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use http::HeaderMap;
use thiserror::Error;

// 送信側 (PR2) でのみ使う import 群。
#[cfg(test)]
use ed25519_dalek::pkcs8::DecodePrivateKey;
#[cfg(test)]
use ed25519_dalek::{Signer, SigningKey};

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("Signature-Input header is empty")]
    Empty,
    #[error("Signature-Input value is malformed: {0}")]
    Malformed(&'static str),
    #[error("covered components inner list is malformed")]
    BadComponentList,
    #[error("signature value is missing `:..:` sf-binary delimiters")]
    BadSfBinary,
    #[error("Signature dictionary has no entry for label {0:?}")]
    LabelNotFound(String),
    #[error("created/expires parameter must be an integer")]
    BadInteger(#[from] std::num::ParseIntError),
}

#[derive(Debug, Error)]
pub(crate) enum BaseError {
    #[error("required header missing from request: {0}")]
    MissingHeader(String),
    #[error("header value is not valid ASCII: {0}")]
    NonAscii(String),
    #[error("derived component {0} is not supported")]
    UnsupportedDerived(String),
}

#[derive(Debug, Error)]
pub(crate) enum VerifyError {
    #[error("public key PEM is invalid: {0}")]
    BadKey(#[from] ed25519_dalek::pkcs8::spki::Error),
    #[error("signature is not valid base64: {0}")]
    BadBase64(#[from] base64::DecodeError),
    #[error("signature length is not 64 bytes (got {0})")]
    BadLength(usize),
    #[error("signature does not verify")]
    BadSignature,
}

/// 送信側 (M3b-2 PR2 で本格使用) のエラー型。現状は単体テストと内部
/// `sign_ed25519` のみが返す。
#[cfg(test)]
#[derive(Debug, Error)]
pub(crate) enum SignError {
    #[error("private key PEM is invalid: {0}")]
    BadKey(#[from] ed25519_dalek::pkcs8::Error),
}

/// パース済み `Signature-Input` の単一ラベル分。
#[derive(Debug, Clone)]
pub(crate) struct SignatureInput<'a> {
    pub label: &'a str,
    /// covered components (順序保持). 例: `["@method", "@target-uri", "host"]`
    pub covered: Vec<&'a str>,
    pub created: Option<i64>,
    pub keyid: Option<&'a str>,
    pub alg: Option<&'a str>,
    /// 元の "値" 部分 (`(...)...;params`) を保持。`@signature-params` の
    /// 行に貼り付けるためにそのまま残す。
    pub raw_value: &'a str,
}

/// `Signature-Input: sig1=("@method" "host");created=...;keyid="..."` をパース。
///
/// 単一ラベルのみサポート (最初の `=` で分割)。
pub(crate) fn parse_signature_input(header: &str) -> Result<SignatureInput<'_>, ParseError> {
    if header.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let (label, value) = header
        .split_once('=')
        .ok_or(ParseError::Malformed("missing '=' between label and value"))?;
    let label = label.trim();
    let value_trimmed = value.trim();

    // 値は `(comp1 comp2 ...);param1=v1;param2=v2` 形式。
    let close_paren = value_trimmed
        .find(')')
        .ok_or(ParseError::Malformed("missing ')' in covered components"))?;
    let open_paren = value_trimmed
        .find('(')
        .ok_or(ParseError::Malformed("missing '(' in covered components"))?;
    if open_paren > close_paren {
        return Err(ParseError::BadComponentList);
    }
    let inside = &value_trimmed[open_paren + 1..close_paren];
    let covered = parse_component_list(inside)?;
    let params_str = &value_trimmed[close_paren + 1..];

    let mut created: Option<i64> = None;
    let mut keyid: Option<&str> = None;
    let mut alg: Option<&str> = None;

    for chunk in params_str.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let (k, v) = chunk
            .split_once('=')
            .ok_or(ParseError::Malformed("parameter missing '='"))?;
        let k = k.trim();
        let v_raw = v.trim();
        // 開き `"` があれば閉じ `"` も必須 (F8)。閉じ忘れを無言で受理しない。
        let v = match v_raw.strip_prefix('"') {
            Some(inner) => inner
                .strip_suffix('"')
                .ok_or(ParseError::Malformed("unterminated quoted parameter value"))?,
            None => v_raw,
        };
        match k {
            "created" => created = Some(v.parse()?),
            "keyid" => keyid = Some(v),
            "alg" => alg = Some(v),
            // expires / nonce / tag 等は M3b-2 では未使用 (将来の clock skew
            // 拡張や replay 対策で取り込む)。今は静かに無視する。
            _ => {}
        }
    }

    Ok(SignatureInput {
        label,
        covered,
        created,
        keyid,
        alg,
        raw_value: value_trimmed,
    })
}

/// `("a" "b" "c")` の内側 `"a" "b" "c"` を `["a", "b", "c"]` に分解。
fn parse_component_list(inside: &str) -> Result<Vec<&str>, ParseError> {
    let mut out = Vec::new();
    let trimmed = inside.trim();
    if trimmed.is_empty() {
        return Ok(out);
    }
    for tok in trimmed.split_whitespace() {
        let name = tok
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .ok_or(ParseError::BadComponentList)?;
        out.push(name);
    }
    Ok(out)
}

/// `Signature: sig1=:<base64>:` から指定ラベルの値を base64 デコードして
/// 生バイトを返す。
pub(crate) fn extract_signature_bytes(header: &str, label: &str) -> Result<Vec<u8>, ParseError> {
    for entry in header.split(',') {
        let entry = entry.trim();
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        if k.trim() != label {
            continue;
        }
        let inner = v
            .trim()
            .strip_prefix(':')
            .and_then(|s| s.strip_suffix(':'))
            .ok_or(ParseError::BadSfBinary)?;
        return B64.decode(inner).map_err(|_| ParseError::BadSfBinary);
    }
    Err(ParseError::LabelNotFound(label.to_string()))
}

/// signature base 文字列を組み立てる (RFC 9421 §2.5)。
///
/// `raw_signature_input_value` は `Signature-Input` ヘッダの **value 部分
/// そのまま** (label `=` の右側) を渡す。これが `@signature-params` 行の
/// 値になる。
pub(crate) fn build_signature_base(
    method: &str,
    target_uri: &str,
    covered: &[&str],
    request_headers: &HeaderMap,
    raw_signature_input_value: &str,
) -> Result<String, BaseError> {
    let mut lines: Vec<String> = Vec::with_capacity(covered.len() + 1);
    for name in covered {
        let value = derive_component(name, method, target_uri, request_headers)?;
        lines.push(format!("\"{name}\": {value}"));
    }
    lines.push(format!(
        "\"@signature-params\": {raw_signature_input_value}"
    ));
    Ok(lines.join("\n"))
}

fn derive_component(
    name: &str,
    method: &str,
    target_uri: &str,
    request_headers: &HeaderMap,
) -> Result<String, BaseError> {
    match name {
        "@method" => Ok(method.to_ascii_uppercase()),
        "@target-uri" => Ok(target_uri.to_string()),
        "@authority" => extract_authority(target_uri, request_headers),
        "@path" => Ok(extract_path(target_uri)),
        "@query" => Ok(extract_query(target_uri)),
        derived if derived.starts_with('@') => {
            Err(BaseError::UnsupportedDerived(derived.to_string()))
        }
        header_name => collect_header(request_headers, header_name),
    }
}

fn extract_authority(target_uri: &str, headers: &HeaderMap) -> Result<String, BaseError> {
    // target_uri の `scheme://authority/path` から authority を引く。
    // 失敗時は Host ヘッダから取る。
    if let Some((_, after_scheme)) = target_uri.split_once("://") {
        let authority = after_scheme.split('/').next().unwrap_or("");
        if !authority.is_empty() {
            return Ok(authority.to_ascii_lowercase());
        }
    }
    collect_header(headers, "host").map(|s| s.to_ascii_lowercase())
}

fn extract_path(target_uri: &str) -> String {
    let after_scheme = target_uri
        .split_once("://")
        .map_or(target_uri, |(_, rest)| rest);
    let path_and_query = after_scheme.split_once('/').map_or("", |(_, rest)| rest);
    let path = path_and_query.split('?').next().unwrap_or("");
    format!("/{path}")
}

fn extract_query(target_uri: &str) -> String {
    target_uri
        .split_once('?')
        .map_or(String::new(), |(_, q)| format!("?{q}"))
}

fn collect_header(headers: &HeaderMap, name: &str) -> Result<String, BaseError> {
    let mut iter = headers.get_all(name).iter().peekable();
    if iter.peek().is_none() {
        return Err(BaseError::MissingHeader(name.to_string()));
    }
    let mut parts: Vec<String> = Vec::new();
    for value in iter {
        let s = value
            .to_str()
            .map_err(|_| BaseError::NonAscii(name.to_string()))?;
        parts.push(s.trim().to_string());
    }
    Ok(parts.join(", "))
}

/// Ed25519 (RFC 8032) で signature base を検証する。
pub(crate) fn verify_ed25519(
    signature_base: &[u8],
    signature_bytes: &[u8],
    ed25519_public_pem: &str,
) -> Result<(), VerifyError> {
    let vk = VerifyingKey::from_public_key_pem(ed25519_public_pem)?;
    let sig_array: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| VerifyError::BadLength(signature_bytes.len()))?;
    let sig = Signature::from_bytes(&sig_array);
    vk.verify(signature_base, &sig)
        .map_err(|_| VerifyError::BadSignature)
}

/// Ed25519 で signature base に署名し、64 バイトの生 signature を返す。
/// インバウンドテスト用 (送出は M3b-3 以降)。
#[cfg(test)]
pub(crate) fn sign_ed25519(
    signature_base: &[u8],
    ed25519_private_pem: &str,
) -> Result<[u8; 64], SignError> {
    let sk = SigningKey::from_pkcs8_pem(ed25519_private_pem)?;
    let sig = sk.sign(signature_base);
    Ok(sig.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use http::{HeaderName, HeaderValue};
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::rand_core::OsRng;

    fn build_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()).unwrap();
            h.append(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn fresh_ed25519_keypair() -> (String, String) {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let pub_pem = vk.to_public_key_pem(LineEnding::LF).unwrap();
        (priv_pem, pub_pem)
    }

    #[test]
    fn parse_typical_signature_input() {
        let header = r#"sig1=("@method" "@target-uri" "host" "date" "content-digest");created=1700000000;keyid="https://example/users/a#ed25519-key";alg="ed25519""#;
        let parsed = parse_signature_input(header).unwrap();
        assert_eq!(parsed.label, "sig1");
        assert_eq!(
            parsed.covered,
            vec!["@method", "@target-uri", "host", "date", "content-digest"]
        );
        assert_eq!(parsed.created, Some(1_700_000_000));
        assert_eq!(parsed.keyid, Some("https://example/users/a#ed25519-key"));
        assert_eq!(parsed.alg, Some("ed25519"));
    }

    #[test]
    fn parse_signature_input_preserves_component_order() {
        // 順序が逆 → covered も逆順になる。signature base 生成で重要。
        let header = r#"sig1=("content-digest" "date" "host" "@target-uri" "@method");keyid="k""#;
        let parsed = parse_signature_input(header).unwrap();
        assert_eq!(
            parsed.covered,
            vec!["content-digest", "date", "host", "@target-uri", "@method"]
        );
    }

    #[test]
    fn parse_signature_input_empty_components() {
        let header = r#"sig1=();keyid="k""#;
        let parsed = parse_signature_input(header).unwrap();
        assert!(parsed.covered.is_empty());
    }

    #[test]
    fn parse_signature_input_rejects_empty() {
        assert!(matches!(
            parse_signature_input("").unwrap_err(),
            ParseError::Empty
        ));
    }

    #[test]
    fn extract_signature_bytes_roundtrip() {
        let sig_bytes = vec![1u8, 2, 3, 4, 5];
        let header = format!("sig1=:{}:", B64.encode(&sig_bytes));
        let out = extract_signature_bytes(&header, "sig1").unwrap();
        assert_eq!(out, sig_bytes);
    }

    #[test]
    fn extract_signature_bytes_label_not_found() {
        let header = "sig1=:AAAA:";
        assert!(matches!(
            extract_signature_bytes(header, "missing").unwrap_err(),
            ParseError::LabelNotFound(ref s) if s == "missing"
        ));
    }

    #[test]
    fn extract_signature_bytes_requires_sf_binary_colons() {
        // sf-binary の `:value:` ラッパが無い → BadSfBinary
        let header = "sig1=AAAA";
        assert!(matches!(
            extract_signature_bytes(header, "sig1").unwrap_err(),
            ParseError::BadSfBinary
        ));
    }

    #[test]
    fn build_signature_base_with_derived_components() {
        let headers = build_headers(&[
            ("Host", "example.com"),
            ("Date", "Tue, 20 Apr 2021 02:07:56 GMT"),
            ("Content-Digest", "sha-256=:ZGlnZXN0OmFiYw==:"),
        ]);
        let raw_value = r#"("@method" "@target-uri" "host" "date" "content-digest");created=1700000000;keyid="k";alg="ed25519""#;
        let base = build_signature_base(
            "POST",
            "https://example.com/inbox",
            &["@method", "@target-uri", "host", "date", "content-digest"],
            &headers,
            raw_value,
        )
        .unwrap();
        let expected = "\
\"@method\": POST\n\
\"@target-uri\": https://example.com/inbox\n\
\"host\": example.com\n\
\"date\": Tue, 20 Apr 2021 02:07:56 GMT\n\
\"content-digest\": sha-256=:ZGlnZXN0OmFiYw==:\n\
\"@signature-params\": (\"@method\" \"@target-uri\" \"host\" \"date\" \"content-digest\");created=1700000000;keyid=\"k\";alg=\"ed25519\"";
        assert_eq!(base, expected);
    }

    #[test]
    fn build_signature_base_authority_from_target_uri() {
        let headers = build_headers(&[]);
        let raw = r#"("@authority");keyid="k""#;
        let base = build_signature_base(
            "POST",
            "https://Example.COM:8443/foo",
            &["@authority"],
            &headers,
            raw,
        )
        .unwrap();
        // @authority は normalized lowercase。
        assert!(base.starts_with("\"@authority\": example.com:8443\n"));
    }

    #[test]
    fn build_signature_base_path_and_query() {
        let headers = build_headers(&[]);
        let raw = r#"("@path" "@query");keyid="k""#;
        let base = build_signature_base(
            "GET",
            "https://example.com/users/a?x=1&y=2",
            &["@path", "@query"],
            &headers,
            raw,
        )
        .unwrap();
        assert!(base.contains("\"@path\": /users/a\n"));
        assert!(base.contains("\"@query\": ?x=1&y=2\n"));
    }

    #[test]
    fn build_signature_base_unsupported_derived() {
        let headers = build_headers(&[]);
        let raw = r#"("@status");keyid="k""#;
        assert!(matches!(
            build_signature_base("POST", "https://x.test/", &["@status"], &headers, raw)
                .unwrap_err(),
            BaseError::UnsupportedDerived(ref n) if n == "@status"
        ));
    }

    #[test]
    fn sign_and_verify_ed25519_roundtrip() {
        let (priv_pem, pub_pem) = fresh_ed25519_keypair();
        let base = b"\"@method\": POST\n\"@target-uri\": https://x.test/inbox\n\"@signature-params\": (\"@method\" \"@target-uri\");keyid=\"k\";alg=\"ed25519\"";
        let sig = sign_ed25519(base, &priv_pem).unwrap();
        verify_ed25519(base, &sig, &pub_pem).unwrap();
    }

    #[test]
    fn verify_ed25519_fails_on_tampered_base() {
        let (priv_pem, pub_pem) = fresh_ed25519_keypair();
        let base = b"original message";
        let sig = sign_ed25519(base, &priv_pem).unwrap();
        let tampered = b"different message";
        assert!(matches!(
            verify_ed25519(tampered, &sig, &pub_pem).unwrap_err(),
            VerifyError::BadSignature
        ));
    }

    #[test]
    fn verify_ed25519_rejects_wrong_signature_length() {
        let (_priv_pem, pub_pem) = fresh_ed25519_keypair();
        assert!(matches!(
            verify_ed25519(b"x", &[1, 2, 3], &pub_pem).unwrap_err(),
            VerifyError::BadLength(3)
        ));
    }

    #[test]
    fn verify_ed25519_rejects_garbage_pem() {
        let garbage = "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n";
        let sig = [0u8; 64];
        assert!(matches!(
            verify_ed25519(b"x", &sig, garbage).unwrap_err(),
            VerifyError::BadKey(_)
        ));
    }
}
