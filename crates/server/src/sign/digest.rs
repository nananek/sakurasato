//! HTTP body の digest 計算と検証。
//!
//! cavage 系は **RFC 3230 形式の `Digest: SHA-256=<base64>`**、RFC 9421
//! 系は **RFC 9530 形式の `Content-Digest: sha-256=:<base64>:`**
//! (Structured Field Dictionary, sf-binary) を用いる。POST inbox では
//! ボディの digest を必須化し、改竄検出と署名 base への組み込みに使う。
//!
//! 比較は [`subtle::ConstantTimeEq`] で定数時間。base64 は標準
//! (`+/=`、padding あり) を使う。両仕様とも `SHA-256` は同じ 32 バイトを
//! 出すので、`expected` の 32 バイトを取り出してから body の SHA-256 と
//! 比較する設計。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum DigestError {
    #[error("digest header is missing")]
    Missing,
    #[error("digest header is malformed")]
    Malformed,
    #[error("digest header has no sha-256 entry")]
    NoSha256,
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("digest length is not 32 bytes (got {0})")]
    BadLength(usize),
    #[error("digest does not match body")]
    Mismatch,
}

/// `body` の SHA-256 を計算し、生 32 バイトで返す。
pub(crate) fn sha256(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hasher.finalize().into()
}

/// cavage 形式 `SHA-256=<base64>` をフォーマットする。
///
/// M3b-2 PR2 (アウトバウンド配送) で本格使用予定。PR1 では統合テストの
/// 署名リクエスト組み立てからのみ使われる。
#[allow(dead_code, reason = "M3b-2 PR2 のアウトバウンド配送で本格使用")]
pub(crate) fn format_cavage(body: &[u8]) -> String {
    format!("SHA-256={}", B64.encode(sha256(body)))
}

/// RFC 9530 形式 `sha-256=:<base64>:` をフォーマットする。送信側で使用。
#[allow(dead_code, reason = "outbound Content-Digest は M3b-3 で利用")]
pub(crate) fn format_content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", B64.encode(sha256(body)))
}

/// cavage `Digest:` ヘッダから SHA-256 ハッシュ (32 バイト) を取り出す。
///
/// 仕様: `Digest: SHA-256=<base64>` 単一エントリ、または
/// `Digest: SHA-256=<b64>, SHA-512=<b64>` のようにカンマ区切りで複数並ぶ
/// 場合がある。本実装は **大文字小文字を無視して** sha-256 エントリを
/// 抽出する (Mastodon は `SHA-256=`、Misskey は `sha-256=` の事例あり)。
fn extract_cavage_sha256(header: &str) -> Result<[u8; 32], DigestError> {
    for entry in header.split(',') {
        let entry = entry.trim();
        let Some((alg, value)) = entry.split_once('=') else {
            continue;
        };
        if alg.trim().eq_ignore_ascii_case("sha-256") {
            let bytes = B64.decode(value.trim())?;
            return decode_into_array(&bytes);
        }
    }
    Err(DigestError::NoSha256)
}

/// RFC 9530 `Content-Digest:` (sf-binary Dictionary) から SHA-256 を取り出す。
///
/// 仕様: `Content-Digest: sha-256=:<base64>:` で値はコロンで囲まれた
/// sf-binary。本実装は他のアルゴリズム (`sha-512`) が併記されていても
/// `sha-256` だけを拾う。鍵名は小文字必須 (RFC 8941 §3.1.2)。
fn extract_content_sha256(header: &str) -> Result<[u8; 32], DigestError> {
    for entry in header.split(',') {
        let entry = entry.trim();
        let Some((alg, value)) = entry.split_once('=') else {
            continue;
        };
        if alg.trim() != "sha-256" {
            continue;
        }
        let inner = value
            .trim()
            .strip_prefix(':')
            .and_then(|v| v.strip_suffix(':'))
            .ok_or(DigestError::Malformed)?;
        let bytes = B64.decode(inner)?;
        return decode_into_array(&bytes);
    }
    Err(DigestError::NoSha256)
}

fn decode_into_array(bytes: &[u8]) -> Result<[u8; 32], DigestError> {
    let len = bytes.len();
    <[u8; 32]>::try_from(bytes).map_err(|_| DigestError::BadLength(len))
}

/// `body` に対する cavage 形式 digest ヘッダを検証する。
pub(crate) fn verify_cavage(body: &[u8], header: Option<&str>) -> Result<(), DigestError> {
    let header = header.ok_or(DigestError::Missing)?;
    let expected = extract_cavage_sha256(header)?;
    let actual = sha256(body);
    if bool::from(expected.ct_eq(&actual)) {
        Ok(())
    } else {
        Err(DigestError::Mismatch)
    }
}

/// `body` に対する RFC 9530 形式 `Content-Digest` ヘッダを検証する。
pub(crate) fn verify_content_digest(body: &[u8], header: Option<&str>) -> Result<(), DigestError> {
    let header = header.ok_or(DigestError::Missing)?;
    let expected = extract_content_sha256(header)?;
    let actual = sha256(body);
    if bool::from(expected.ct_eq(&actual)) {
        Ok(())
    } else {
        Err(DigestError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    // base64 standard = 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=
    const EMPTY_SHA256_B64: &str = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

    #[test]
    fn sha256_known_empty_vector() {
        let h = sha256(b"");
        assert_eq!(B64.encode(h), EMPTY_SHA256_B64);
    }

    #[test]
    fn format_cavage_empty_body() {
        assert_eq!(format_cavage(b""), format!("SHA-256={EMPTY_SHA256_B64}"));
    }

    #[test]
    fn format_content_digest_empty_body() {
        assert_eq!(
            format_content_digest(b""),
            format!("sha-256=:{EMPTY_SHA256_B64}:")
        );
    }

    #[test]
    fn verify_cavage_roundtrip() {
        let body = br#"{"type":"Create"}"#;
        let header = format_cavage(body);
        verify_cavage(body, Some(&header)).unwrap();
    }

    #[test]
    fn verify_cavage_tampered_body_fails() {
        let body = br#"{"type":"Create"}"#;
        let header = format_cavage(body);
        let tampered = br#"{"type":"Delete"}"#;
        assert!(matches!(
            verify_cavage(tampered, Some(&header)).unwrap_err(),
            DigestError::Mismatch
        ));
    }

    #[test]
    fn verify_cavage_missing_header() {
        assert!(matches!(
            verify_cavage(b"", None).unwrap_err(),
            DigestError::Missing
        ));
    }

    #[test]
    fn verify_cavage_case_insensitive_alg() {
        // Misskey 実装は `sha-256=...` (小文字) を送る事例がある。
        let body = b"hello";
        let h = B64.encode(sha256(body));
        let header = format!("sha-256={h}");
        verify_cavage(body, Some(&header)).unwrap();
    }

    #[test]
    fn verify_cavage_multi_algorithm_picks_sha256() {
        let body = b"hello";
        let h = B64.encode(sha256(body));
        // 仮の sha-512 を後ろに付ける。実装が sha-256 だけを拾うことを確認。
        let header = format!("SHA-256={h}, SHA-512=irrelevantvalue");
        verify_cavage(body, Some(&header)).unwrap();
    }

    #[test]
    fn verify_content_digest_roundtrip() {
        let body = br#"{"type":"Create"}"#;
        let header = format_content_digest(body);
        verify_content_digest(body, Some(&header)).unwrap();
    }

    #[test]
    fn verify_content_digest_requires_colons() {
        // sf-binary は `:value:` で囲む必要がある。コロン欠落は Malformed。
        let body = b"hello";
        let h = B64.encode(sha256(body));
        let header = format!("sha-256={h}"); // コロン無し
        assert!(matches!(
            verify_content_digest(body, Some(&header)).unwrap_err(),
            DigestError::Malformed
        ));
    }

    #[test]
    fn verify_content_digest_tampered_body_fails() {
        let body = br#"{"type":"Create"}"#;
        let header = format_content_digest(body);
        let tampered = br#"{"type":"Delete"}"#;
        assert!(matches!(
            verify_content_digest(tampered, Some(&header)).unwrap_err(),
            DigestError::Mismatch
        ));
    }

    #[test]
    fn verify_content_digest_no_sha256_entry() {
        // RFC 9530 では `sha-256` 鍵が小文字必須 (RFC 8941 §3.1.2)。
        // 大文字 `SHA-256` は拾わない。
        let body = b"hello";
        let h = B64.encode(sha256(body));
        let header = format!("SHA-256=:{h}:");
        assert!(matches!(
            verify_content_digest(body, Some(&header)).unwrap_err(),
            DigestError::NoSha256
        ));
    }

    #[test]
    fn verify_content_digest_bad_base64() {
        let header = "sha-256=:!!!notbase64!!!:";
        assert!(matches!(
            verify_content_digest(b"", Some(header)).unwrap_err(),
            DigestError::Base64(_)
        ));
    }
}
