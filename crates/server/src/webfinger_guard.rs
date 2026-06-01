//! `WebFinger` 経路の cross-domain hijack 防御 (PR #78 review F-1)。
//!
//! `WebFinger` サーバは「`acct:user@example.com` を `https://attacker.example/users/x` に
//! 向ける」差し替えを返せる。`fetch_and_upsert` 内の `id == ap_id` 自己整合性
//! チェックでは attacker 側 actor が自分の id を正しく返すので攻撃を検出できない。
//! 本モジュールは「クエリした acct のホストと `actor_uri` のホストが一致するか」
//! だけを担保する。
//!
//! 元々 [`crate::follow`] と [`crate::local_api::notes`] にそれぞれ複製があり、
//! M13 PR1 (Issue #79) で `GET /api/v1/actor` に同じ防御を入れるため共通化した。

use anyhow::{Context, bail};

/// `acct` (= `user@host` / `@user@host` / `acct:user@host`) から host 部だけを
/// lower-case で抜き出す。形式が壊れていれば `None`。
///
/// `media-proxy::webfinger::parse_acct` と同じ受理形を踏襲する。
pub fn extract_acct_host(acct: &str) -> Option<String> {
    let trimmed = acct.trim();
    let body = trimmed
        .strip_prefix("acct:")
        .unwrap_or(trimmed)
        .trim_start_matches('@');
    let (_, host) = body.split_once('@')?;
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// `WebFinger` が返した `actor_uri` のホストが、クエリした acct のホストと
/// 一致するかを検証する。不一致は cross-domain 差し替え攻撃の徴候として拒否する。
///
/// `url::Host` は DNS 名を lowercase で返す。IP リテラル等は [`crate::net_guard`]
/// で別途遮断されるので、ここはドメイン文字列の一致だけを担保する。
pub fn ensure_webfinger_host_match(expected_host_lc: &str, actor_uri: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(actor_uri)
        .with_context(|| format!("WebFinger returned invalid actor_uri {actor_uri:?}"))?;
    let actor_host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("WebFinger actor_uri {actor_uri:?} has no host"))?
        .to_ascii_lowercase();
    if actor_host == expected_host_lc {
        return Ok(());
    }
    bail!(
        "WebFinger returned actor_uri on different host (expected {expected_host_lc:?}, \
         got {actor_host:?}); possible cross-domain redirect, refusing to use",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_acct_host_handles_all_three_forms() {
        assert_eq!(
            extract_acct_host("acct:alice@Example.com").as_deref(),
            Some("example.com"),
        );
        assert_eq!(
            extract_acct_host("@alice@example.com").as_deref(),
            Some("example.com"),
        );
        assert_eq!(
            extract_acct_host("alice@example.com").as_deref(),
            Some("example.com"),
        );
        assert_eq!(extract_acct_host("no-at-sign"), None);
        assert_eq!(extract_acct_host("alice@"), None);
    }

    #[test]
    fn ensure_webfinger_host_match_rejects_cross_domain() {
        ensure_webfinger_host_match("evil.example", "https://evil.example/users/bob").unwrap();
        let err = ensure_webfinger_host_match("evil.example", "https://victim.example/users/bob")
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("different host"), "msg={msg}");
        assert!(msg.contains("victim.example"), "msg={msg}");
    }

    #[test]
    fn ensure_webfinger_host_match_is_case_insensitive() {
        ensure_webfinger_host_match("evil.example", "https://EVIL.EXAMPLE/users/bob").unwrap();
    }

    #[test]
    fn ensure_webfinger_host_match_rejects_invalid_uri() {
        let err = ensure_webfinger_host_match("a.test", "not a url at all").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid actor_uri"), "msg={msg}");
    }
}
