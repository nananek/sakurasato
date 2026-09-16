//! 外向き HTTP クライアント用の DNS resolver ガード。
//!
//! [`sakurasato_core::net_guard::host_blocked`] は URL 文字列しか見ないため、
//! `*.nip.io` のような wildcard DNS、Docker の単一ラベル名 (`versitygw`)、
//! Tailscale `MagicDNS` (`*.ts.net`) を経由した private / loopback / link-local /
//! CGNAT への到達を防げない。本モジュールは `reqwest::dns::Resolve` を実装し、
//! **解決後の全 IP** を [`sakurasato_core::net_guard`] の IP 判定にかける。
//!
//! resolver が返したアドレスだけが接続に使われるため、DNS 解決と接続の間で
//! 結果が差し替わる TOCTOU (DNS rebinding) も成立しない。
//!
//! `allow_private` は本番では常に `false`。テスト / Docker 内の連合テスト
//! (`SAKURASATO_ALLOW_PRIVATE_EGRESS=1`) だけが明示的に `true` に倒す。
//! `AppState::from_pool` (テスト経路) は loopback のダミー inbox を立てるため
//! 常に `true` を使う。

use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sakurasato_core::net_guard;

/// 解決後 IP を検証する resolver。
#[derive(Debug, Clone)]
pub(crate) struct GuardedResolver {
    /// `true` のとき IP 検証をスキップする (テスト / 連合テスト専用)。
    allow_private: bool,
}

impl GuardedResolver {
    /// `Arc` 化した resolver を作る (`ClientBuilder::dns_resolver` が要求する形)。
    pub(crate) fn new(allow_private: bool) -> Arc<Self> {
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
    // port は reqwest が URL 側の値で上書きする契約
    // (`Resolve` trait の doc: "Explicitly specified port in the URL will
    // override any port in the resolved SocketAddr's")。
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
        // `localhost` は resolver / hosts で 127.0.0.1 / ::1 に解決される。
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
