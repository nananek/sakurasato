//! `keyId` 文字列を `(ap_id, fragment)` に分解し、`Sakurasato` 規約の
//! `#main-key` (RSA) / `#ed25519-key` (Ed25519) を判定する。
//!
//! cavage は `Signature` ヘッダの `keyId="..."` パラメタ、RFC 9421 は
//! `Signature-Input` の `keyid="..."` パラメタにそれぞれ載る。どちらも
//! `<actor-ap-id>#<fragment>` 形式の URI で、本モジュールはこの分解を
//! 一手に引き受ける。
//!
//! `ap_id` 側のホスト一致検証 (なりすまし object 注入対策) は remote actor
//! を実際に fetch する [`crate::sign`] 上位レイヤの仕事で、本モジュールは
//! 純粋な文字列分解と種別判定だけを行う。

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum KeyIdError {
    #[error("keyId is empty")]
    Empty,
    #[error("keyId has no '#' fragment separator")]
    NoFragment,
    #[error("keyId has empty actor id before '#'")]
    EmptyActor,
    #[error("keyId has empty fragment after '#'")]
    EmptyFragment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedKeyId<'a> {
    pub ap_id: &'a str,
    pub fragment: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    /// Sakurasato 規約の `#main-key` (Mastodon / Misskey 等の主流互換)。
    Rsa,
    /// Sakurasato 規約の `#ed25519-key` (Nekonoverse / FEP-521a 互換)。
    Ed25519,
    /// 上記以外の fragment (`#legacy-key` 等)。M3b-2 では未対応として扱う。
    Other,
}

impl ParsedKeyId<'_> {
    pub(crate) fn kind(&self) -> KeyKind {
        match self.fragment {
            "main-key" => KeyKind::Rsa,
            "ed25519-key" => KeyKind::Ed25519,
            _ => KeyKind::Other,
        }
    }
}

/// `<ap_id>#<fragment>` を分解する。
///
/// - `keyId` に `#` が含まれない、または fragment が空 → エラー
/// - `#` 直前 (`ap_id`) が空 → エラー
/// - `#` が複数ある場合は **最初の `#`** で分割 (`URI` fragment は 1 個のみ)。
pub(crate) fn parse(key_id: &str) -> Result<ParsedKeyId<'_>, KeyIdError> {
    if key_id.is_empty() {
        return Err(KeyIdError::Empty);
    }
    let (ap_id, fragment) = key_id.split_once('#').ok_or(KeyIdError::NoFragment)?;
    if ap_id.is_empty() {
        return Err(KeyIdError::EmptyActor);
    }
    if fragment.is_empty() {
        return Err(KeyIdError::EmptyFragment);
    }
    Ok(ParsedKeyId { ap_id, fragment })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rsa_main_key() {
        let p = parse("https://example.com/users/alice#main-key").unwrap();
        assert_eq!(p.ap_id, "https://example.com/users/alice");
        assert_eq!(p.fragment, "main-key");
        assert_eq!(p.kind(), KeyKind::Rsa);
    }

    #[test]
    fn parse_ed25519_key() {
        let p = parse("https://nekonoverse/users/bob#ed25519-key").unwrap();
        assert_eq!(p.ap_id, "https://nekonoverse/users/bob");
        assert_eq!(p.fragment, "ed25519-key");
        assert_eq!(p.kind(), KeyKind::Ed25519);
    }

    #[test]
    fn parse_other_fragment_kind() {
        // 規約外の fragment は `Other` に倒す。M3b-2 では未対応扱い。
        let p = parse("https://example.com/users/c#legacy-key").unwrap();
        assert_eq!(p.kind(), KeyKind::Other);
    }

    #[test]
    fn parse_empty_string() {
        assert_eq!(parse(""), Err(KeyIdError::Empty));
    }

    #[test]
    fn parse_no_fragment() {
        assert_eq!(
            parse("https://example.com/users/alice"),
            Err(KeyIdError::NoFragment),
        );
    }

    #[test]
    fn parse_empty_actor() {
        assert_eq!(parse("#main-key"), Err(KeyIdError::EmptyActor));
    }

    #[test]
    fn parse_empty_fragment() {
        assert_eq!(
            parse("https://example.com/users/alice#"),
            Err(KeyIdError::EmptyFragment),
        );
    }

    #[test]
    fn parse_uses_first_hash() {
        // URI 仕様上 fragment は 1 個のみだが、現実には変なデータも来うる。
        // `split_once('#')` は最初の `#` で割るので、後続の `#` は fragment
        // 側に残る (= `Other` kind になる)。挙動を固定するためのテスト。
        let p = parse("https://example.com/users/alice#main-key#extra").unwrap();
        assert_eq!(p.ap_id, "https://example.com/users/alice");
        assert_eq!(p.fragment, "main-key#extra");
        assert_eq!(p.kind(), KeyKind::Other);
    }
}
