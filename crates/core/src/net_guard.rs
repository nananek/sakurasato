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
//! - IPv4 = グローバルユニキャスト以外。loopback / private / link-local /
//!   documentation / CGNAT に加え、IETF protocol assignment・benchmarking・
//!   multicast・将来用途の予約範囲も遮断する
//! - IPv6 = loopback / 未指定 / multicast / link-local (`fe80::/10`) /
//!   unique-local (`fc00::/7`) / documentation (`2001:db8::/32`, `3fff::/20`) /
//!   site-local (`fec0::/10`) / ORCHID (`2001:10::/28`, `2001:20::/28`) /
//!   discard-only (`100::/64`) / benchmarking (`2001:2::/48`) /
//!   `5f00::/16`。加えて **埋め込み IPv4 を持つ移行アドレス**
//!   (NAT64 `64:ff9b::/96`、6to4 `2002::/16`、Teredo `2001::/32`) は
//!   埋め込み IPv4 を IPv4 ルールで再検査し、private / loopback / 特殊なら
//!   遮断する (public への NAT64 は IPv6-only 環境の正規経路なので許可)。
//!   deprecated な IPv4-compatible `::a.b.c.d` は一律遮断。
//!
//! ## ホスト名の正規化
//!
//! `url::Url::host_str()` は FQDN の末尾ドットを保持する (`evil.example.`)。
//! DB 保存・モデレーション照合・[`is_self_host`] は [`canonical_host`] を
//! 通し、末尾ドット付きの別名でガードをすり抜けられないようにする。
//!
//! ドメイン側は RFC 6761 `localhost.` / `*.localhost`、`localhost.localdomain`、
//! RFC 6762 mDNS `.local`、および **ドットを含まない単一ラベルのホスト名**
//! (docker compose の internal network サービス名 `postgres`/`versitygw` 等と
//! 衝突し得るため) を遮断する。
//!
//! ## なぜ完全な SSRF 対策ではないか
//!
//! 本モジュールの [`host_blocked`] は DNS 解決の **前** の URL 文字列だけを
//! 見る。`evil.example` の A レコードが `127.0.0.1` に向いている (= DNS
//! rebinding) ケースは単体では防げないため、外向き HTTP クライアントは
//! **custom DNS resolver で解決後の全 IP を [`filter_resolved_addrs`] に
//! 通す** こと (server / media-proxy の `dns_guard` が実装)。これで
//! wildcard DNS (`*.nip.io`)・Docker 単一ラベル名・Tailscale `MagicDNS` を
//! 経由した private / loopback / link-local / CGNAT への到達を塞ぐ。
//!
//! TUI 側からは redirect ごとに本関数を再呼び出しして「リダイレクト先も検証
//! 漏れしない」原則を担保する (PR #35 claude-review 指摘)。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

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

/// テスト / Docker 内連合テスト専用のオプトイン環境変数。
///
/// `"1"` / `"true"` (ASCII 大文字小文字無視) のときだけ、外向き HTTP の
/// resolver が private / loopback / link-local / CGNAT への接続を許可する。
/// **本番では設定しないこと** ── 設定すると [`host_blocked`] と resolver の
/// 二段ガードが両方とも緩む。
pub const ALLOW_PRIVATE_EGRESS_ENV: &str = "SAKURASATO_ALLOW_PRIVATE_EGRESS";

/// 環境変数 [`ALLOW_PRIVATE_EGRESS_ENV`] から `allow_private` を解決する。
/// 未設定 / それ以外の値は `false` (安全側)。
pub fn allow_private_egress_from_env() -> bool {
    allow_private_egress_value(std::env::var(ALLOW_PRIVATE_EGRESS_ENV).ok().as_deref())
}

fn allow_private_egress_value(raw: Option<&str>) -> bool {
    raw.is_some_and(|v| v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true"))
}

/// `ip` 単体の遮断判定。[`host_blocked`] の IP リテラル判定と **同じ規則** を
/// 解決後アドレスにも適用するための公開エントリポイント。
///
/// 外向き HTTP クライアントの custom DNS resolver は、解決結果の各 IP を
/// 本関数にかけ、`Some(reason)` のアドレスを接続候補から除外する。
pub fn ip_blocked(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => ipv4_block_reason(v4),
        IpAddr::V6(v6) => ipv6_block_reason(v6),
    }
}

/// DNS 解決済みアドレス列を検証し、接続に使ってよいものだけを返す。
///
/// - `allow_private` が `false` (本番) のとき、[`ip_blocked`] が `Some` を
///   返すアドレスは除外する。**全アドレスが除外された場合は `Err(reason)`**
///   ── 呼び出し側 (resolver) は接続を拒否する。
/// - `allow_private` が `true` (テスト / Docker 内連合テスト) のときは
///   無検証で全件返す。明示的なオプトインでのみ緩む。
///
/// public / private が混在する場合は public だけを残す (= 攻撃者ドメインが
/// private IP を混ぜて rebinding するケースでも、private 側は使われない)。
pub fn filter_resolved_addrs<I>(
    addrs: I,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, &'static str>
where
    I: IntoIterator<Item = SocketAddr>,
{
    if allow_private {
        return Ok(addrs.into_iter().collect());
    }
    let mut blocked: Option<&'static str> = None;
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|addr| match ip_blocked(addr.ip()) {
            Some(reason) => {
                blocked.get_or_insert(reason);
                false
            }
            None => true,
        })
        .collect();
    if allowed.is_empty()
        && let Some(reason) = blocked
    {
        return Err(reason);
    }
    Ok(allowed)
}

/// ホスト名を比較・保存用に正規化する (小文字化 + 末尾ドット除去)。
///
/// `url::Url::host_str()` は FQDN の末尾ドットを保持するため、同じ実体でも
/// `evil.example` と `evil.example.` が別文字列になる。ドメインモデレーション
/// (`domain_moderation.host`) や `is_self_host` がこれを取りこぼすと、
/// ブロック済みドメインが別名で素通りする。ホスト文字列を DB に書くとき・
/// 照合するときは必ず本関数を通すこと。
pub fn canonical_host(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `url` の host が `server_host` (本インスタンスの公開ホスト名) と
/// 大文字小文字 / 末尾ドットを無視して一致するか。
pub fn is_self_host(url: &url::Url, server_host: &str) -> bool {
    url.host_str()
        .is_some_and(|h| canonical_host(h) == canonical_host(server_host))
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
    // 単一ラベル (ドットを含まない) ホスト名は、正当な公開 Fediverse ドメイン
    // としてはあり得ない (実在する TLD 直下の 1 ラベルドメインは事実上存在
    // しない) 一方、docker compose のサービス名 (`postgres` / `versitygw` /
    // `media-proxy` 等、internal network 上で DNS 解決される) や社内ホスト名
    // とは一致し得る。`GET /media-proxy` ([[media-proxy-miauth]]) を無認証で
    // 公開した結果、この経路が internal network 限定サービスへの到達性
    // プロービングに使われ得るため、ここで遮断して多層防御を足す。
    if !lower.contains('.') {
        return Some("single-label-host");
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
    } else if in_ipv4_cidr(ip, Ipv4Addr::UNSPECIFIED, 8) {
        Some("this-network")
    } else if in_ipv4_cidr(ip, Ipv4Addr::new(192, 0, 0, 0), 24) {
        Some("ietf-protocol-assignment")
    } else if in_ipv4_cidr(ip, Ipv4Addr::new(192, 88, 99, 0), 24) {
        Some("deprecated-6to4-relay")
    } else if in_ipv4_cidr(ip, Ipv4Addr::new(198, 18, 0, 0), 15) {
        Some("benchmarking")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if u32::from(ip) & 0xF000_0000 == 0xF000_0000 {
        Some("reserved")
    } else {
        None
    }
}

/// RFC 6598 `100.64.0.0/10` (CGNAT) の判定。
fn is_ipv4_cgnat(ip: Ipv4Addr) -> bool {
    u32::from(ip) & 0xFFC0_0000 == 0x6440_0000
}

/// IPv4 CIDR 内包判定。`prefix_len` は本モジュール内の定数だけを渡す。
fn in_ipv4_cidr(ip: Ipv4Addr, network: Ipv4Addr, prefix_len: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - prefix_len).unwrap_or(0);
    u32::from(ip) & mask == u32::from(network) & mask
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
    if (segs[0] & 0xffc0) == 0xfec0 {
        // fec0::/10 site-local (RFC 3879 で廃止)。現行 OS でも内部扱い。
        return Some("site-local");
    }
    if segs[0] == 0x2001 && segs[1] == 0x0db8 {
        return Some("documentation");
    }
    if (segs[0] & 0xfff0) == 0x3ff0 {
        // 3fff::/20 (RFC 9637 で追加された documentation 用)。
        return Some("documentation");
    }
    if segs[0] == 0x5f00 {
        // 5f00::/16 (SRv6 SID、グローバル到達性なし)。
        return Some("segment-routing");
    }
    if segs[0] == 0x0100 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0 {
        // 100::/64 (RFC 6666 discard-only)。
        return Some("discard-only");
    }
    if segs[0] == 0x2001 && (segs[1] & 0xfff0) == 0x0010 {
        // 2001:10::/28 ORCHID (RFC 4843)。
        return Some("orchid");
    }
    if segs[0] == 0x2001 && (segs[1] & 0xfff0) == 0x0020 {
        // 2001:20::/28 ORCHIDv2 (RFC 7343)。
        return Some("orchid");
    }
    if segs[0] == 0x2001 && segs[1] == 0x0002 && segs[2] == 0 {
        // 2001:2::/48 benchmarking (RFC 5180)。
        return Some("benchmarking");
    }
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001 {
        // 64:ff9b:1::/48 local-use NAT64 (RFC 8215)。インターネット向けの
        // 正規経路ではないため一律遮断。
        return Some("nat64-local-use");
    }
    // 埋め込み IPv4 を持つ移行アドレスは、埋め込み IPv4 を IPv4 ルールで
    // 再検査する。private / loopback / 特殊用途なら遮断し、public なら
    // 許可する (IPv6-only + NAT64 環境の正規トラフィックを壊さない)。
    if let Some((v4, kind)) = embedded_ipv4(&ip) {
        return ipv4_block_reason(v4).map(|_| match kind {
            "nat64" => "nat64-private",
            "6to4" => "6to4-private",
            _ => "teredo-private",
        });
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_block_reason(v4);
    }
    if ip.to_ipv4().is_some() {
        // deprecated な IPv4-compatible `::a.b.c.d` (:: と ::1、::ffff:0:0/96
        // は上で処理済み)。ルーティング可能な正規用途が無いため一律遮断。
        return Some("ipv4-compatible");
    }
    None
}

/// IPv6 の移行アドレスに埋め込まれた IPv4 を取り出す。
///
/// - NAT64 well-known prefix `64:ff9b::/96` (RFC 6052): 下位 32bit
/// - 6to4 `2002::/16` (RFC 3056): bit 16..48
/// - Teredo `2001::/32` (RFC 4380): 下位 32bit が obfuscated client IPv4
fn embedded_ipv4(ip: &Ipv6Addr) -> Option<(Ipv4Addr, &'static str)> {
    let segs = ip.segments();
    if segs[0] == 0x0064
        && segs[1] == 0xff9b
        && segs[2] == 0
        && segs[3] == 0
        && segs[4] == 0
        && segs[5] == 0
    {
        return Some((v4_from_segs(segs[6], segs[7]), "nat64"));
    }
    if segs[0] == 0x2002 {
        return Some((v4_from_segs(segs[1], segs[2]), "6to4"));
    }
    if segs[0] == 0x2001 && segs[1] == 0x0000 {
        let obfuscated = (u32::from(segs[6]) << 16) | u32::from(segs[7]);
        return Some((Ipv4Addr::from(!obfuscated), "teredo"));
    }
    None
}

fn v4_from_segs(hi: u16, lo: u16) -> Ipv4Addr {
    #[allow(clippy::cast_possible_truncation)] // 16bit を上下 8bit に分割
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
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
    fn blocks_non_global_ipv4_special_use_ranges() {
        for (address, reason) in [
            (Ipv4Addr::new(0, 1, 2, 3), "this-network"),
            (Ipv4Addr::new(192, 0, 0, 42), "ietf-protocol-assignment"),
            (Ipv4Addr::new(192, 88, 99, 1), "deprecated-6to4-relay"),
            (Ipv4Addr::new(198, 18, 0, 1), "benchmarking"),
            (Ipv4Addr::new(198, 19, 255, 254), "benchmarking"),
            (Ipv4Addr::new(224, 0, 0, 1), "multicast"),
            (Ipv4Addr::new(239, 255, 255, 255), "multicast"),
            (Ipv4Addr::new(240, 0, 0, 1), "reserved"),
            (Ipv4Addr::new(255, 255, 255, 254), "reserved"),
        ] {
            assert_eq!(ip_blocked(IpAddr::V4(address)), Some(reason), "{address}");
        }
    }

    #[test]
    fn allows_global_unicast_adjacent_to_special_use_ranges() {
        for address in [
            Ipv4Addr::new(1, 0, 0, 0),
            Ipv4Addr::new(192, 0, 1, 1),
            Ipv4Addr::new(198, 17, 255, 255),
            Ipv4Addr::new(198, 20, 0, 0),
            Ipv4Addr::new(223, 255, 255, 255),
        ] {
            assert_eq!(ip_blocked(IpAddr::V4(address)), None, "{address}");
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
    fn allow_private_egress_env_value_is_strict() {
        assert!(!allow_private_egress_value(None));
        assert!(!allow_private_egress_value(Some("")));
        assert!(!allow_private_egress_value(Some("0")));
        assert!(!allow_private_egress_value(Some("no")));
        assert!(allow_private_egress_value(Some("1")));
        assert!(allow_private_egress_value(Some("true")));
        assert!(allow_private_egress_value(Some("TRUE")));
    }

    #[test]
    fn ip_blocked_matches_host_blocked_for_literals() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert_eq!(
            ip_blocked(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Some("loopback")
        );
        assert_eq!(
            ip_blocked(IpAddr::V4(Ipv4Addr::new(100, 100, 0, 1))),
            Some("cgnat-shared")
        );
        assert_eq!(
            ip_blocked(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            Some("loopback")
        );
        assert_eq!(ip_blocked(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))), None);
    }

    #[test]
    fn filter_resolved_addrs_drops_private_and_keeps_public() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let addrs = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443),
        ];
        let filtered = filter_resolved_addrs(addrs, false).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].ip().to_string(), "1.1.1.1");
    }

    #[test]
    fn filter_resolved_addrs_rejects_all_private() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        // Docker 内部名 (`versitygw`) や `*.nip.io` が private に解決される
        // ケースを想定した全滅パターン。
        let addrs = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 80),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 100, 0, 1)), 80),
        ];
        assert_eq!(filter_resolved_addrs(addrs, false), Err("private"),);
    }

    #[test]
    fn filter_resolved_addrs_allows_private_when_opted_in() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let addrs = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 80)];
        let filtered = filter_resolved_addrs(addrs, true).unwrap();
        assert_eq!(filtered.len(), 1);
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

    /// docker compose の internal network 上でだけ解決されるサービス名
    /// (`postgres` / `versitygw` / `media-proxy` 等) はドットを含まない
    /// 単一ラベルなので遮断する。無認証で公開した `GET /media-proxy`
    /// ([[media-proxy-miauth]]) 経由の到達性プロービング対策。
    #[test]
    fn blocks_single_label_hostnames() {
        for s in [
            "http://postgres/x",
            "http://versitygw:7070/x",
            "http://media-proxy/x",
            "http://internalhost/x",
        ] {
            assert_eq!(host_blocked(&url(s)), Some("single-label-host"), "{s}");
        }
    }

    /// 通常のドット入りドメインは (この規則では) 引き続き通過する。
    #[test]
    fn allows_multi_label_hostnames() {
        assert_eq!(host_blocked(&url("https://mastodon.social/x")), None);
        assert_eq!(host_blocked(&url("https://a.b.c/x")), None);
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

    #[test]
    fn self_host_matches_trailing_dot_alias() {
        // `url` crate は末尾ドットを保持するため、末尾ドット付きの別名で
        // 自己配送ループガードをすり抜けられないことを担保する。
        assert!(is_self_host(
            &url("https://example.test./inbox"),
            "example.test"
        ));
        assert!(is_self_host(
            &url("https://EXAMPLE.test./x"),
            "example.test."
        ));
    }

    #[test]
    fn canonical_host_normalizes_case_and_trailing_dot() {
        assert_eq!(canonical_host("Example.TEST."), "example.test");
        assert_eq!(canonical_host("evil.example"), "evil.example");
        assert_eq!(canonical_host("evil.example..."), "evil.example");
        assert_eq!(canonical_host("  EVIL.example. "), "evil.example");
    }

    #[test]
    fn blocks_ipv6_special_use_ranges() {
        for s in [
            "http://[fec0::1]/x",
            "http://[100::1]/x",
            "http://[2001:10::1]/x",
            "http://[2001:20::1]/x",
            "http://[2001:2::1]/x",
            "http://[3fff::1]/x",
            "http://[5f00::1]/x",
            "http://[64:ff9b:1::1]/x",
            "http://[::7f00:1]/x",
        ] {
            assert!(host_blocked(&url(s)).is_some(), "{s} must be blocked");
        }
    }

    #[test]
    fn blocks_transition_addresses_embedding_private_ipv4() {
        // ATK: NAT64 / 6to4 / Teredo で private / IMDS へ翻訳させる経路。
        for s in [
            "http://[64:ff9b::7f00:1]/x",          // 127.0.0.1
            "http://[64:ff9b::a9fe:a9fe]/x",       // 169.254.169.254
            "http://[64:ff9b::a00:1]/x",           // 10.0.0.1
            "http://[2002:7f00:1::]/x",            // 6to4 127.0.0.1
            "http://[2002:c0a8:101::]/x",          // 6to4 192.168.1.1
            "http://[2001:0:0:0:0:0:80ff:fffe]/x", // Teredo client 127.0.0.1
        ] {
            assert!(host_blocked(&url(s)).is_some(), "{s} must be blocked");
        }
    }

    #[test]
    fn allows_transition_addresses_embedding_public_ipv4() {
        // IPv6-only + NAT64 環境では public IPv4 サイトが 64:ff9b::/96 に
        // 解決される。公開アドレスへの正規経路は許可する。
        assert_eq!(host_blocked(&url("http://[64:ff9b::1.1.1.1]/x")), None);
        assert_eq!(host_blocked(&url("http://[2002:101:101::]/x")), None);
    }
}
