//! 外部 GET 用 `reqwest::Client`。
//!
//! media-proxy はこのクライアントだけが外部 (= compose `egress` ネット越し
//! のインターネット) に出る。SSRF / redirect / timeout の防御を一箇所に集約
//! することで、handler 側は「URL を渡せば安全に bytes が返ってくる」契約に
//! 専念できる。
//!
//! ## redirect 再検証
//!
//! [`Policy::custom`] で各 hop ごとに [`host_blocked`] を再呼び出しする。
//! 配送相手が `Location: http://169.254.169.254/...` を返した場合の SSRF を
//! ここで遮断する。最大 3 hop。
//!
//! ## DNS 解決後の IP 検証
//!
//! `net_guard::host_blocked` は URL 文字列 (= ホスト名) しか見ていないため、
//! `evil.example` の A レコードが `127.0.0.1` を返す (= DNS rebinding)、
//! `*.nip.io` が private IP を返す、Docker 内部名 (`versitygw`) や
//! Tailscale `MagicDNS` 名が内部 IP に解決される、といった経路は文字列検査
//! だけでは防げない。本クライアントは [`crate::dns_guard::GuardedResolver`] を
//! `dns_resolver` として登録し、**解決後の全 IP** を IP 判定にかける ──
//! resolver が返したアドレスだけが接続に使われるので TOCTOU も無い。
//!
//! `allow_private` は Docker 内の連合テスト専用
//! (`SAKURASATO_ALLOW_PRIVATE_EGRESS=1`)。本番経路は常に `false`。

use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use reqwest::redirect::Policy;
use sakurasato_core::net_guard::host_blocked;

use crate::dns_guard::GuardedResolver;

/// リクエスト全体の timeout (DNS + TLS handshake + body 受信込み)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP connect timeout 単独の上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// redirect 追従の最大 hop。3 hop あれば普通のリダイレクト経路はカバーできる。
const MAX_REDIRECTS: usize = 3;

/// 連合相手のアクセスログから判別できるよう `sakurasato-media-proxy/<version>` を載せる。
const USER_AGENT: &str = concat!(
    "sakurasato-media-proxy/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/nananek/sakurasato)"
);

/// 外部 GET 用クライアントを構築する。SSRF 防御 + redirect 検証 +
/// 解決後 IP 検証付き。
///
/// `allow_private` はテスト / Docker 内連合テスト専用のオプトイン
/// (`SAKURASATO_ALLOW_PRIVATE_EGRESS`)。本番は `false` を渡す。
pub fn build_client(allow_private: bool) -> anyhow::Result<Client> {
    let redirect = Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(format!("too many redirects (>{MAX_REDIRECTS})"));
        }
        if !allow_private && let Some(reason) = host_blocked(attempt.url()) {
            return attempt.error(format!("redirect to blocked host ({reason})"));
        }
        attempt.follow()
    });

    Client::builder()
        .user_agent(USER_AGENT)
        .redirect(redirect)
        // 環境 proxy に宛先ホストの DNS 解決を委ねると GuardedResolver を
        // 迂回できるため、SSRF guard 対象の取得では proxy を無効化する。
        .no_proxy()
        .dns_resolver(GuardedResolver::new(allow_private))
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("build reqwest::Client for media-proxy outbound")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_succeeds() {
        let _client = build_client(false).unwrap();
        let _client_allow_private = build_client(true).unwrap();
    }

    #[test]
    fn user_agent_includes_project_marker() {
        assert!(USER_AGENT.starts_with("sakurasato-media-proxy/"));
        assert!(USER_AGENT.contains("github.com/nananek/sakurasato"));
    }
}
