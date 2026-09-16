//! 外向き HTTP クライアント用の DNS resolver ガード (media-proxy 版)。
//!
//! server 側 (`crates/server/src/dns_guard.rs`) と **同じ方針** を実装する:
//! `reqwest::dns::Resolve` で解決後の全 IP を
//! [`sakurasato_core::net_guard`] の IP 判定にかけ、private / loopback /
//! link-local / CGNAT への接続を resolver 段で拒否する。`host_blocked` の
//! URL 文字列検査だけでは wildcard DNS (`*.nip.io`) / 単一ラベル名 /
//! Tailscale `MagicDNS` を経由した内部到達を防げないため、media-proxy 側でも
//! 必ず本 resolver を使うこと。
//!
//! 方針 (IP 判定 + `allow_private` の意味) は core の
//! [`sakurasato_core::net_guard::filter_resolved_addrs`] が単一の情報源。
//! `allow_private` は Docker 内の連合テスト専用で、本番は常に `false`。

use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sakurasato_core::net_guard;

/// 解決後 IP を検証する resolver。
#[derive(Debug, Clone)]
pub struct GuardedResolver {
    /// `true` のとき IP 検証をスキップする (テスト / 連合テスト専用)。
    allow_private: bool,
}

impl GuardedResolver {
    /// `Arc` 化した resolver を作る (`ClientBuilder::dns_resolver` が要求する形)。
    pub fn new(allow_private: bool) -> Arc<Self> {
        Arc::new(Self { allow_private })
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(resolve_guarded(
            name.as_str().to_string(),
            self.allow_private,
        ))
    }
}

/// `Resolving` の `BoxError` に `io::Error` を変換しつつ解決 + 検証する本体。
async fn resolve_guarded(
    host: String,
    allow_private: bool,
) -> Result<Addrs, Box<dyn std::error::Error + Send + Sync>> {
    // port は reqwest が URL 側の値で上書きする契約。
    let addrs = tokio::net::lookup_host((host.as_str(), 0))
        .await
        .map_err(|e| std::io::Error::other(format!("dns lookup {host}: {e}")))?;
    let filtered = net_guard::filter_resolved_addrs(addrs, allow_private).map_err(|reason| {
        std::io::Error::other(format!(
            "dns lookup {host}: every resolved address is blocked ({reason})"
        ))
    })?;
    if filtered.is_empty() {
        return Err(
            std::io::Error::other(format!("dns lookup {host}: no addresses returned")).into(),
        );
    }
    Ok(Box::new(filtered.into_iter()) as Addrs)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn resolve_once(allow_private: bool, host: &str) -> Result<Vec<SocketAddr>, String> {
        let resolver = GuardedResolver { allow_private };
        let name: Name = host.parse().map_err(|e| format!("parse name: {e:?}"))?;
        let fut = resolver.resolve(name);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(fut)
            .map(Iterator::collect)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn blocks_loopback_resolution_by_default() {
        let err = resolve_once(false, "localhost").unwrap_err();
        assert!(
            err.contains("blocked") || err.contains("no addresses"),
            "loopback must be refused, got: {err}"
        );
    }

    #[test]
    fn allows_loopback_when_opted_in() {
        let addrs = resolve_once(true, "localhost").expect("allow_private must permit loopback");
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }
}
