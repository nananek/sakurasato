//! 外向き HTTP 先の URL ガード (SSRF 最小防御)。
//!
//! [`crate::delivery`] のアウトバウンド配送と [`crate::remote_actor`] の
//! actor fetch で **同じガード**を通す。配送と fetch で別関数になっていると
//! どちらかにだけ防御を忘れる事故が起きやすい ── M3b-3 PR2 round で
//! `inbox_host_blocked` を共有化することを明示的に決定した
//! (CLAUDE.md §3 / [[m3b-followup-plan]])。
//!
//! 役割:
//! - [`host_blocked`] : URL の host が IP literal または `localhost` /
//!   `*.local` 等の予約ドメインなら理由を返す。ドメイン名 (一般 TLD)
//!   は通過させる ── DNS 解決後の判定は media-proxy 側 (CLAUDE.md §3)。
//! - [`is_self_host`] : URL の host が自インスタンスの公開ホスト名と
//!   一致するか。配送ループ防止と自己 fetch 抑止に使う。
//!
//! IPv4 / IPv6 の遮断レンジは [[m3b-followup-plan]] round-1/2 で確定したもの:
//! IPv4 = loopback / private / link-local (169.254/16 メタデータ) / 未指定 /
//!        broadcast / documentation / RFC 6598 CGNAT 100.64.0.0/10
//! IPv6 = loopback / 未指定 / multicast / link-local (`fe80::/10`) /
//!        unique-local (`fc00::/7`) / documentation (`2001:db8::/32`) /
//!        IPv4-mapped で埋め込み IPv4 が private な場合
//!
//! ドメイン側は RFC 6761 `localhost.` / `*.localhost`、`localhost.localdomain`、
//! RFC 6762 mDNS `.local` を遮断する (round-2 F2)。
//!
//! ## なぜ完全な SSRF 対策ではないか
//!
//! 本モジュールは DNS 解決の **前** の URL 文字列だけを見る。`evil.example`
//! の A レコードが `127.0.0.1` に向いている (= DNS rebinding) ケースは
//! 防げない。これは server 直の暫定構成 (#23) の既知制限で、CLAUDE.md §3
//! の再評価項目 (b) に記録済み。本格的な多層防御 (custom connector で
//! socket レベル検証) は media-proxy 経由に移行する際に実装する。

use std::net::{Ipv4Addr, Ipv6Addr};

/// `url` の host が外向き HTTP の宛先として **遮断すべき** 範囲なら、
/// その理由を `&'static str` で返す。通過させてよい場合は `None`。
///
/// 配送先 inbox URL と remote actor fetch URL の両方をこの関数で検査する
/// ことが M3b-3 PR2 で確定した不変条件。
pub(crate) fn host_blocked(url: &reqwest::Url) -> Option<&'static str> {
    match url.host()? {
        url::Host::Ipv4(ip) => ipv4_block_reason(ip),
        url::Host::Ipv6(ip) => ipv6_block_reason(ip),
        url::Host::Domain(d) => domain_block_reason(d),
    }
}

/// `url` の host が `server_host` (本インスタンスの公開ホスト名) と
/// 大文字小文字を区別せず一致するか。
///
/// 配送経路では「自己宛配送ループ」防止、actor fetch では「自分自身の
/// actor を外部 GET で取りに行く」明らかな意味不明動作の抑止に使う。
pub(crate) fn is_self_host(url: &reqwest::Url, server_host: &str) -> bool {
    url.host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case(server_host))
}

/// ループバックまたは LAN 内に解決される可能性のあるドメイン名なら、
/// その理由を返す。
///
/// RFC 6761 §6.3 が `localhost.` および `.localhost.` 配下の名前を
/// ループバック専用として予約している ── DNS 解決を待たずにここで弾く。
/// `localhost.localdomain` は古い Linux ディストリの慣習名で、
/// `/etc/hosts` で 127.0.0.1 に張られていることが多いため同様に拒否する。
/// RFC 6762 (mDNS) の `.local` TLD は Avahi / systemd-resolved が動く
/// 環境で LAN 内の任意ホストに解決されるため、server が egress を持つ
/// 暫定構成 (#23) では SSRF ベクタになり得る。
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
    // `lower` は to_ascii_lowercase 済みなので、`ends_with` は実質的に
    // 大文字小文字を区別しない比較になっている。clippy の
    // `case_sensitive_file_extension_comparisons` は `.local` を拡張子と
    // 誤検知するため allow する。
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

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
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
