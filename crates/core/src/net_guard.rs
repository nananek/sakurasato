//! 外向き HTTP 先の URL ガード (SSRF 最小防御)。
//!
//! M3b-3 で server クレートに導入した同名モジュール (配送 / actor fetch 用) を
//! M5 PR2 (TUI 画像取得) からも再利用するため `core` に昇格させた。配送 /
//! fetch / TUI のいずれかにだけ防御を忘れる事故を防ぐ ── これが本モジュールが
//! `core` に居る最大の理由。
//!
//! 役割:
//! - [`host_blocked`] : URL の host が IP literal または `localhost` /
//!   `*.local` 等の予約ドメインなら理由を返す。ドメイン名 (一般 TLD)
//!   は通過させる (= DNS 解決後の判定は呼び出し側 / media-proxy)。
//! - [`is_self_host`] : URL の host が自インスタンスの公開ホスト名と
//!   一致するか。配送ループ防止と自己 fetch 抑止に使う。
//!
//! IPv4 / IPv6 の遮断レンジ:
//! - IPv4 = loopback / private / link-local (169.254/16 メタデータ) / 未指定 /
//!   broadcast / documentation / RFC 6598 CGNAT 100.64.0.0/10
//! - IPv6 = loopback / 未指定 / multicast / link-local (`fe80::/10`) /
//!   unique-local (`fc00::/7`) / documentation (`2001:db8::/32`) /
//!   IPv4-mapped で埋め込み IPv4 が private な場合
//!
//! ドメイン側は RFC 6761 `localhost.` / `*.localhost`、`localhost.localdomain`、
//! RFC 6762 mDNS `.local` を遮断する。
//!
//! ## なぜ完全な SSRF 対策ではないか
//!
//! 本モジュールは DNS 解決の **前** の URL 文字列だけを見る。`evil.example`
//! の A レコードが `127.0.0.1` に向いている (= DNS rebinding) ケースは
//! 防げない。これは server 直の暫定構成 (#23) の既知制限。本格的な多層防御
//! (custom connector で socket レベル検証) は media-proxy 経由に移行する際に
//! 実装する。
//!
//! TUI 側からは redirect ごとに本関数を再呼び出しして「リダイレクト先も検証
//! 漏れしない」原則を担保する (PR #35 claude-review 指摘)。

use std::net::{Ipv4Addr, Ipv6Addr};

/// `url` の host が外向き HTTP の宛先として **遮断すべき** 範囲なら、
/// その理由を `&'static str` で返す。通過させてよい場合は `None`。
///
/// reqwest を呼び出す側 (`crates/server` / `crates/tui`) が同関数を共有する
/// ことが M3b-3 PR2 で確定した不変条件。
pub fn host_blocked(url: &url::Url) -> Option<&'static str> {
    match url.host()? {
        url::Host::Ipv4(ip) => ipv4_block_reason(ip),
        url::Host::Ipv6(ip) => ipv6_block_reason(ip),
        url::Host::Domain(d) => domain_block_reason(d),
    }
}

/// `url` の host が `server_host` (本インスタンスの公開ホスト名) と
/// 大文字小文字を区別せず一致するか。
pub fn is_self_host(url: &url::Url, server_host: &str) -> bool {
    url.host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case(server_host))
}

fn domain_block_reason(domain: &str) -> Option<&'static str> {
    let trimmed = domain.trim_end_matches('.');
    if trimmed.eq_ignore_ascii_case("localhost")
        || trimmed.eq_ignore_ascii_case("localhost.localdomain")
    {
        return Some("localhost-domain");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with(".localhost") {
        return Some("localhost-domain");
    }
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if lower == "local" || lower.ends_with(".local") {
        return Some("mdns-local");
    }
    None
}

fn ipv4_block_reason(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_private() {
        Some("private")
    } else if ip.is_link_local() {
        Some("link-local")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_broadcast() {
        Some("broadcast")
    } else if ip.is_documentation() {
        Some("documentation")
    } else if is_ipv4_cgnat(ip) {
        Some("cgnat-shared")
    } else {
        None
    }
}

/// RFC 6598 `100.64.0.0/10` (CGNAT) の判定。
fn is_ipv4_cgnat(ip: Ipv4Addr) -> bool {
    u32::from(ip) & 0xFFC0_0000 == 0x6440_0000
}

fn ipv6_block_reason(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback");
    }
    if ip.is_unspecified() {
        return Some("unspecified");
    }
    if ip.is_multicast() {
        return Some("multicast");
    }
    let segs = ip.segments();
    if (segs[0] & 0xffc0) == 0xfe80 {
        return Some("link-local");
    }
    if (segs[0] & 0xfe00) == 0xfc00 {
        return Some("unique-local");
    }
    if segs[0] == 0x2001 && segs[1] == 0x0db8 {
        return Some("documentation");
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_block_reason(v4);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn blocks_ipv4_loopback() {
        assert_eq!(host_blocked(&url("http://127.0.0.1/x")), Some("loopback"));
    }

    #[test]
    fn blocks_ipv4_private_ranges() {
        for s in [
            "http://10.0.0.1/x",
            "http://172.16.5.5/x",
            "http://192.168.1.1/x",
        ] {
            assert_eq!(host_blocked(&url(s)), Some("private"), "{s}");
        }
    }

    #[test]
    fn blocks_ipv4_link_local_metadata() {
        // 169.254.169.254 はクラウド IMDS。
        assert_eq!(
            host_blocked(&url("http://169.254.169.254/latest/meta-data/")),
            Some("link-local")
        );
    }

    #[test]
    fn blocks_ipv4_cgnat() {
        for s in [
            "http://100.64.0.1/x",
            "http://100.100.0.1/x",
            "http://100.127.255.254/x",
        ] {
            assert_eq!(host_blocked(&url(s)), Some("cgnat-shared"), "{s}");
        }
    }

    #[test]
    fn allows_ipv4_adjacent_to_cgnat() {
        assert_eq!(host_blocked(&url("http://100.63.255.255/x")), None);
        assert_eq!(host_blocked(&url("http://100.128.0.0/x")), None);
    }

    #[test]
    fn blocks_ipv4_unspecified_and_broadcast() {
        assert_eq!(host_blocked(&url("http://0.0.0.0/x")), Some("unspecified"));
        assert_eq!(
            host_blocked(&url("http://255.255.255.255/x")),
            Some("broadcast")
        );
    }

    #[test]
    fn blocks_ipv6_loopback_link_local_unique_local() {
        assert_eq!(host_blocked(&url("http://[::1]/x")), Some("loopback"));
        assert_eq!(host_blocked(&url("http://[fe80::1]/x")), Some("link-local"));
        assert_eq!(
            host_blocked(&url("http://[fc00::1]/x")),
            Some("unique-local")
        );
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6_private() {
        assert_eq!(
            host_blocked(&url("http://[::ffff:c0a8:0101]/x")),
            Some("private")
        );
    }

    #[test]
    fn allows_public_ipv4_literal() {
        assert_eq!(host_blocked(&url("http://1.1.1.1/x")), None);
    }

    #[test]
    fn allows_domain_name() {
        assert_eq!(host_blocked(&url("https://mastodon.example/x")), None);
        assert_eq!(host_blocked(&url("https://mylocalhost.example/x")), None);
        assert_eq!(host_blocked(&url("https://localhost.example.com/x")), None);
    }

    #[test]
    fn blocks_localhost_domain() {
        assert_eq!(
            host_blocked(&url("http://localhost/x")),
            Some("localhost-domain")
        );
        assert_eq!(
            host_blocked(&url("http://LOCALHOST/x")),
            Some("localhost-domain")
        );
        assert_eq!(
            host_blocked(&url("http://app.localhost/x")),
            Some("localhost-domain")
        );
        assert_eq!(
            host_blocked(&url("http://a.b.localhost/x")),
            Some("localhost-domain")
        );
        assert_eq!(
            host_blocked(&url("http://localhost.localdomain/x")),
            Some("localhost-domain")
        );
    }

    #[test]
    fn blocks_mdns_local_tld() {
        assert_eq!(
            host_blocked(&url("http://postgres.local/x")),
            Some("mdns-local")
        );
        assert_eq!(
            host_blocked(&url("http://server.lan.local/x")),
            Some("mdns-local")
        );
        assert_eq!(host_blocked(&url("http://LOCAL/x")), Some("mdns-local"));
    }

    #[test]
    fn allows_non_local_tlds() {
        assert_eq!(host_blocked(&url("https://localhost.com/x")), None);
        assert_eq!(host_blocked(&url("https://mylocal.example/x")), None);
        assert_eq!(host_blocked(&url("https://site.locally/x")), None);
    }

    #[test]
    fn self_host_match_is_case_insensitive() {
        assert!(is_self_host(&url("https://example.test/x"), "example.test"));
        assert!(is_self_host(
            &url("https://EXAMPLE.test/u/x/inbox"),
            "example.test"
        ));
        assert!(is_self_host(
            &url("http://example.test:8080/x"),
            "example.test"
        ));
    }

    #[test]
    fn self_host_rejects_subdomain() {
        assert!(!is_self_host(&url("https://other.test/x"), "example.test"));
        assert!(!is_self_host(
            &url("https://sub.example.test/x"),
            "example.test"
        ));
    }
}
