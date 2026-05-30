//! cavage HTTP signatures draft-12 のパーサと署名 base 生成、RSA-SHA256
//! の署名/検証。
//!
//! Mastodon / Misskey / Fedibird など Fediverse 主流実装は本 draft (Expired
//! だが依然 de-facto standard) に準拠した `Signature` ヘッダを用いる。
//!
//! 署名 base の正規化規則 (draft-12 §2.3):
//! - 各行は `lowercase(name) + ": " + value`
//! - 擬似ヘッダ `(request-target)` の値は `lowercase(method) + " " + path-and-query`
//! - 擬似ヘッダ `(created)` / `(expires)` の値は 10 進整数 (引用符なし)
//! - 行と行は `\n` で連結、**末尾に `\n` を付けない**
//! - 値の前後 OWS (optional whitespace) は除去
//! - 同名ヘッダが複数ある場合は `, ` で連結 (HTTP セマンティクスに準拠)
//!
//! `headers` パラメタが省略された場合、draft-12 のデフォルトは `(created)`
//! のみだが、Mastodon は実装上 `(request-target) host date` 等を必須化する
//! ため、本サーバの inbox 検証では headers の妥当性 (host / date / digest
//! 必須) を上位の [`crate::sign`] レイヤで強制する。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use http::HeaderMap;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("Signature header is empty")]
    Empty,
    #[error("Signature header parameter is malformed: {0}")]
    Malformed(String),
    #[error("Signature header is missing mandatory parameter: {0}")]
    MissingMandatory(&'static str),
    #[error("created/expires parameter must be an integer")]
    BadInteger(#[from] std::num::ParseIntError),
}

#[derive(Debug, Error)]
pub(crate) enum BaseError {
    #[error("required header missing from request: {0}")]
    MissingHeader(String),
    #[error("header value is not valid ASCII: {0}")]
    NonAscii(String),
    #[error("(created) was covered but no created= parameter was supplied")]
    CreatedRequired,
    #[error("(expires) was covered but no expires= parameter was supplied")]
    ExpiresRequired,
}

#[derive(Debug, Error)]
pub(crate) enum VerifyError {
    #[error("public key PEM is invalid: {0}")]
    BadKey(#[from] rsa::pkcs8::spki::Error),
    #[error("signature is not valid base64: {0}")]
    BadBase64(#[from] base64::DecodeError),
    #[error("signature bytes are malformed: {0}")]
    BadSignature(#[from] rsa::signature::Error),
}

/// 送信側のエラー型。M3b-2 PR2 のアウトバウンド配送で使用。
#[derive(Debug, Error)]
pub(crate) enum SignError {
    #[error("private key PEM is invalid: {0}")]
    BadKey(#[from] rsa::pkcs8::Error),
}

/// パース済み cavage Signature ヘッダ。値は元文字列を借用。
///
/// `algorithm` は Mastodon 系で実質無視される (keyId fragment から鍵種別を
/// 推定するため) ので、parse はするが verify 時には使わない。互換上必要
/// なため struct には残す。
#[derive(Debug, Clone)]
pub(crate) struct SignatureHeader<'a> {
    pub key_id: &'a str,
    #[allow(
        dead_code,
        reason = "Mastodon 系互換のため parse はするが verify では未使用"
    )]
    pub algorithm: Option<&'a str>,
    pub headers: Vec<&'a str>,
    pub signature_b64: &'a str,
    pub created: Option<i64>,
    pub expires: Option<i64>,
}

/// cavage `Signature:` ヘッダの値をパースする。
///
/// 各パラメタは `key="value"` (文字列) または `key=12345` (数値) で
/// カンマ区切り。signature 値は base64 で `,` を含まないため、雑な
/// カンマ分割で誤動作しない。
pub(crate) fn parse_signature_header(header: &str) -> Result<SignatureHeader<'_>, ParseError> {
    if header.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut key_id: Option<&str> = None;
    let mut algorithm: Option<&str> = None;
    let mut headers: Vec<&str> = Vec::new();
    let mut signature_b64: Option<&str> = None;
    let mut created: Option<i64> = None;
    let mut expires: Option<i64> = None;

    for part in header.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| ParseError::Malformed(part.to_string()))?;
        let k = k.trim();
        let v_raw = v.trim();
        // 文字列値は `"..."` で囲まれている。created/expires は引用符無し。
        // 開き `"` があれば閉じ `"` も必須 (F8): `keyId="https://x` のような
        // 閉じ忘れを無言で `https://x` として受理すると、攻撃者が任意の
        // keyId を仕込める。strict にパースする。
        let v = match v_raw.strip_prefix('"') {
            Some(inner) => inner.strip_suffix('"').ok_or_else(|| {
                ParseError::Malformed(format!("unterminated quoted value for {k}"))
            })?,
            None => v_raw,
        };
        match k {
            "keyId" => key_id = Some(v),
            "algorithm" => algorithm = Some(v),
            "headers" => headers = v.split_whitespace().collect(),
            "signature" => signature_b64 = Some(v),
            "created" => created = Some(v.parse()?),
            "expires" => expires = Some(v.parse()?),
            _ => { /* unknown extension; ignore per draft-12 §2.1.6 */ }
        }
    }

    let key_id = key_id.ok_or(ParseError::MissingMandatory("keyId"))?;
    let signature_b64 = signature_b64.ok_or(ParseError::MissingMandatory("signature"))?;
    // headers パラメタ省略時のデフォルトは draft-12 §2.1.3 で `(created)` のみ。
    if headers.is_empty() {
        headers = vec!["(created)"];
    }
    Ok(SignatureHeader {
        key_id,
        algorithm,
        headers,
        signature_b64,
        created,
        expires,
    })
}

/// signature base 文字列を組み立てる。
///
/// `covered` は lowercase されたヘッダ名 (擬似ヘッダ含む) を呼び出し側で
/// 用意する。`request_headers` は HTTP request の実ヘッダ群。
pub(crate) fn build_signature_base(
    method: &str,
    path_and_query: &str,
    covered: &[&str],
    request_headers: &HeaderMap,
    created: Option<i64>,
    expires: Option<i64>,
) -> Result<String, BaseError> {
    let mut lines: Vec<String> = Vec::with_capacity(covered.len());
    for name in covered {
        let value = match *name {
            "(request-target)" => {
                format!("{} {}", method.to_ascii_lowercase(), path_and_query)
            }
            "(created)" => created.ok_or(BaseError::CreatedRequired)?.to_string(),
            "(expires)" => expires.ok_or(BaseError::ExpiresRequired)?.to_string(),
            header_name => collect_header(request_headers, header_name)?,
        };
        lines.push(format!("{name}: {value}"));
    }
    Ok(lines.join("\n"))
}

/// 同名ヘッダの複数値を `, ` で連結し、前後 OWS を除去する。
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

/// RSA-SHA256 (RSASSA-PKCS1-v1_5) で署名 base を検証する。
///
/// `rsa_public_pem` の前後 whitespace は内部で `.trim()` する: Pleroma 2.5.5
/// 等の実装は `publicKeyPem` 末尾に `\n\n` を入れて送ってくることがあり、
/// `rsa::pkcs8` の PEM parser はこれを `PreEncapsulationBoundary` で拒否する
/// ので、保存側で trim していても二重で防御する。
pub(crate) fn verify_rsa_sha256(
    signature_base: &[u8],
    signature_b64: &str,
    rsa_public_pem: &str,
) -> Result<(), VerifyError> {
    let public_key = RsaPublicKey::from_public_key_pem(rsa_public_pem.trim())?;
    let verifier = VerifyingKey::<Sha256>::new(public_key);
    let sig_bytes = B64.decode(signature_b64)?;
    let sig = Signature::try_from(sig_bytes.as_slice())?;
    verifier.verify(signature_base, &sig)?;
    Ok(())
}

/// RSA-SHA256 で署名 base に署名し、生バイトを返す。送信側 (M3b-2 PR2) と
/// インバウンドテストで使用。
pub(crate) fn sign_rsa_sha256(
    signature_base: &[u8],
    rsa_private_pem: &str,
) -> Result<Vec<u8>, SignError> {
    let private_key = rsa::RsaPrivateKey::from_pkcs8_pem(rsa_private_pem)?;
    let signer = SigningKey::<Sha256>::new(private_key);
    let sig = signer.sign(signature_base);
    Ok(sig.to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::pkcs8::LineEnding;
    use rsa::rand_core::OsRng;

    fn fresh_rsa_keypair() -> (String, String) {
        // 2048-bit はテストで遅すぎるので 1024-bit。**プロダクションでは
        // 2048+ を使うこと** (init.rs はそうしている)。
        let priv_key = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
        let pub_key = priv_key.to_public_key();
        let priv_pem = priv_key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let pub_pem = pub_key.to_public_key_pem(LineEnding::LF).unwrap();
        (priv_pem, pub_pem)
    }

    fn build_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            // HeaderName::from_bytes は lifetime 制約が無く、to_lowercase
            // して `IntoHeaderName` の owned 経路を通す。
            let name = HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()).unwrap();
            h.append(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parse_typical_mastodon_signature() {
        // 実 Mastodon が送る形 (見やすさのため空白を入れた)。
        let header = r#"keyId="https://mastodon.example/users/alice#main-key",algorithm="rsa-sha256",headers="(request-target) host date digest",signature="abc123=""#;
        let parsed = parse_signature_header(header).unwrap();
        assert_eq!(
            parsed.key_id,
            "https://mastodon.example/users/alice#main-key"
        );
        assert_eq!(parsed.algorithm, Some("rsa-sha256"));
        assert_eq!(
            parsed.headers,
            vec!["(request-target)", "host", "date", "digest"]
        );
        assert_eq!(parsed.signature_b64, "abc123=");
        assert_eq!(parsed.created, None);
    }

    #[test]
    fn parse_with_created_param() {
        // draft-12 §2.1.4 / §2.1.5: created/expires は引用符無しの整数。
        let header = r#"keyId="kid",signature="sig",created=1700000000,expires=1700000300"#;
        let parsed = parse_signature_header(header).unwrap();
        assert_eq!(parsed.created, Some(1_700_000_000));
        assert_eq!(parsed.expires, Some(1_700_000_300));
        // headers 省略 → デフォルト `(created)`
        assert_eq!(parsed.headers, vec!["(created)"]);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(matches!(
            parse_signature_header("").unwrap_err(),
            ParseError::Empty
        ));
    }

    #[test]
    fn parse_rejects_missing_keyid() {
        let header = r#"algorithm="rsa-sha256",signature="x""#;
        assert!(matches!(
            parse_signature_header(header).unwrap_err(),
            ParseError::MissingMandatory("keyId")
        ));
    }

    #[test]
    fn parse_rejects_missing_signature() {
        let header = r#"keyId="kid",algorithm="rsa-sha256""#;
        assert!(matches!(
            parse_signature_header(header).unwrap_err(),
            ParseError::MissingMandatory("signature")
        ));
    }

    #[test]
    fn parse_ignores_unknown_extension() {
        // 仕様: 未知パラメタは無視 (draft-12 §2.1.6)。
        let header = r#"keyId="kid",signature="sig",fooBar="qux""#;
        parse_signature_header(header).unwrap();
    }

    #[test]
    fn build_signature_base_minimal() {
        // 既知ベクタ: draft-12 §C.1 (Default Test) 相当のミニ版。
        let headers = build_headers(&[
            ("Host", "example.com"),
            ("Date", "Sun, 05 Jan 2014 21:31:40 GMT"),
        ]);
        let base = build_signature_base(
            "POST",
            "/foo?bar=baz",
            &["(request-target)", "host", "date"],
            &headers,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            base,
            "(request-target): post /foo?bar=baz\n\
             host: example.com\n\
             date: Sun, 05 Jan 2014 21:31:40 GMT"
        );
    }

    #[test]
    fn build_signature_base_method_lowercased() {
        let headers = build_headers(&[("Host", "x.test")]);
        let base = build_signature_base(
            "GET",
            "/foo",
            &["(request-target)", "host"],
            &headers,
            None,
            None,
        )
        .unwrap();
        assert!(base.starts_with("(request-target): get /foo\n"));
    }

    #[test]
    fn build_signature_base_trims_ows() {
        // ヘッダ値の前後空白は除去される (draft-12 §2.3 step 3)。
        let headers = build_headers(&[("X-Foo", "  bar baz  ")]);
        let base = build_signature_base("POST", "/", &["x-foo"], &headers, None, None).unwrap();
        assert_eq!(base, "x-foo: bar baz");
    }

    #[test]
    fn build_signature_base_multi_value_joined() {
        // 同名ヘッダが複数あれば `, ` で連結。
        let headers = build_headers(&[("X-Foo", "a"), ("X-Foo", "b")]);
        let base = build_signature_base("POST", "/", &["x-foo"], &headers, None, None).unwrap();
        assert_eq!(base, "x-foo: a, b");
    }

    #[test]
    fn build_signature_base_missing_header() {
        let headers = build_headers(&[]);
        assert!(matches!(
            build_signature_base("POST", "/", &["host"], &headers, None, None).unwrap_err(),
            BaseError::MissingHeader(ref n) if n == "host"
        ));
    }

    #[test]
    fn build_signature_base_created_required() {
        let headers = build_headers(&[]);
        assert!(matches!(
            build_signature_base("POST", "/", &["(created)"], &headers, None, None).unwrap_err(),
            BaseError::CreatedRequired
        ));
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let (priv_pem, pub_pem) = fresh_rsa_keypair();
        let base = b"(request-target): post /inbox\nhost: example\n";
        let sig_bytes = sign_rsa_sha256(base, &priv_pem).unwrap();
        let sig_b64 = B64.encode(sig_bytes);
        verify_rsa_sha256(base, &sig_b64, &pub_pem).unwrap();
    }

    #[test]
    fn verify_fails_on_tampered_base() {
        let (priv_pem, pub_pem) = fresh_rsa_keypair();
        let base = b"(request-target): post /inbox\nhost: example\n";
        let sig_b64 = B64.encode(sign_rsa_sha256(base, &priv_pem).unwrap());
        let tampered = b"(request-target): post /admin\nhost: example\n";
        assert!(matches!(
            verify_rsa_sha256(tampered, &sig_b64, &pub_pem).unwrap_err(),
            VerifyError::BadSignature(_)
        ));
    }

    #[test]
    fn verify_fails_on_bad_base64() {
        let (_priv_pem, pub_pem) = fresh_rsa_keypair();
        assert!(matches!(
            verify_rsa_sha256(b"base", "!!!notbase64!!!", &pub_pem).unwrap_err(),
            VerifyError::BadBase64(_)
        ));
    }

    /// 回帰: Pleroma 2.5.5 が `publicKey.publicKeyPem` を末尾 `\n\n` で
    /// 送ってくるケース。`rsa::pkcs8` の PEM parser は trailing newline 二重に
    /// 厳しく、対応していないと `PreEncapsulationBoundary` で reject されて
    /// 署名検証が `BadKey` → 上位で `BadSignature` にマスクされてしまう。
    /// この入力で verify が通ることを保証する (= 該当 `\n\n` を内部 trim する
    /// 防御を確認)。
    ///
    /// データは 2026-05-30 の実 Pleroma → sakurasato Follow から採取
    /// ([sakurasato#38](https://github.com/nananek/sakurasato/issues/38))。
    #[test]
    fn verify_accepts_pleroma_style_pem_with_double_trailing_newline() {
        use base64::Engine as _;
        let base = base64::engine::general_purpose::STANDARD
            .decode("KHJlcXVlc3QtdGFyZ2V0KTogcG9zdCAvdXNlcnMvbWUvaW5ib3gKY29udGVudC1sZW5ndGg6IDM2MgpkYXRlOiBTYXQsIDMwIE1heSAyMDI2IDExOjIzOjEyIEdNVApkaWdlc3Q6IFNIQS0yNTY9dTFzWnVLVlgrZHc1NWZHZG1jT3BPN3I0aFdKdllKUW5BMkhrblVaMVcxdz0KaG9zdDogc2FrdXJhc2F0bw==")
            .unwrap();
        let sig_b64 = "N2fsNx4l8Qr7deTrrBCMGOxim4ShVKoIaVR4n85HkMmIeRbb9QB+xcTcxJ8WBqoel0yBkn3weZ0JP1NuBt3giaMCzRjPM+ZUVEbZCn5y/K+wpNzWcayAFH7tiLnUHFQnKzQOw+ZkIcUvIqIR6adsxM74gKWBgGZfL7jK0GghaSS07+aLwB88olcrLM6jaL4I5uAv5m3kFP9pYJCKvofXlH1fe5r9wBdN8MSPl5/GtuTDD5LPYW1jBZDSfe93fLfoHXdng/sX1Irn6DHWaWuVtMUy6SlOfNKG90mGPXFAXXHxJMvhlHj3GMGTJOL/TIcj1r2kD/NFVcRlcKkHjRdveA==";
        // Pleroma が送ってきた PEM をそのまま (末尾 `\n\n` 込み) で投げる。
        let pleroma_pem = "-----BEGIN PUBLIC KEY-----\n\
            MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqbO6eu7kXAeiEiUZ6joq\n\
            9Kt8aR5Q96aMSFL+wkxY/Ny9qNcF2dZ73roJi0rtbMcmWIbiXoyC3t+wHvZK9YZ2\n\
            chemq3ULKxmpkZz9rMig9CHDYQJmkjeeoamPlNunGOD20lLHWwggNWs5y5qgweRD\n\
            Kw1Bgl2+SfNY/WBJfLH86wdqfxRhCUtBJ5qgs322eO5rGaYs051whEdyKTfXmp+g\n\
            qjqoqOYBe2CzpPWrknk1JVcRNULA1xcSxIhr5ycEd8vBDpM4yKpuygHlbGh5t4/5\n\
            qaZOZWrQIxHaEQVyCNCP8V/6jOFMhPuFVP+DP+u2wCCDwin3CL3GHlx4UcUdAb+n\n\
            0QIDAQAB\n\
            -----END PUBLIC KEY-----\n\n";
        verify_rsa_sha256(&base, sig_b64, pleroma_pem).unwrap();
    }
}
