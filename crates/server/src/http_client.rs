//! 外向き HTTP クライアント (`reqwest::Client`)。
//!
//! 主な用途は **M3b-2 PR2 のアウトバウンド配送** — 自インスタンスから
//! `/inbox` 等の外部 `ActivityPub` サーバへの POST。
//!
//! **外部 URL の取得 (`GET`) には使わない**。CLAUDE.md §3 / §5.3 で
//! 「外部 URL 取得は必ず media-proxy 経由」と定めており、SSRF 対策・
//! CIDR allowlist・redirect 再検証は media-proxy 側の責務 [[CLAUDE.md §5.3]]。
//! M3b-3 以降で remote actor fetch / OGP 取得を実装する際も、server からは
//! **media-proxy への Unix ソケット呼び出し**にこのクライアントは使わず、
//! 専用の hyper クライアント (or `tokio::net::UnixStream` 経由) を別に用意する
//! 想定。本クライアントを生 `reqwest` のまま `GET` に流用してはいけない。
//!
//! # 安全側に倒した設定
//!
//! - **redirect 無効** (`redirect::Policy::none()`): `ActivityPub` の配送先 URL は
//!   信頼境界の外。3xx 追従を許すと SSRF (内部ネット upgrade) や TOFU
//!   署名検証 (Date / Host が変わる) のループホールを開ける。M3b-3 で remote
//!   actor fetch を実装する段でも生 reqwest は使わず、media-proxy 経由で
//!   ホスト検証付き解決させる方針 (CLAUDE.md §3)。
//! - **timeout / `connect_timeout` 明示**: 上流が無応答でも配送ワーカが詰まらない
//!   よう、リクエスト全体 30s / 接続 10s を上限にする。
//! - **HTTP/2 有効 + keep-alive**: 1 つの相手に複数 Note を立て続けに送る
//!   ケースで TLS handshake を共有でき、レイテンシを抑える。
//! - **User-Agent 明示**: 連合相手のログから `sakurasato/x.y.z` と判別できる
//!   ようにする (Fediverse のマナー)。プロジェクト URL も付ける。

use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use reqwest::redirect::Policy;

/// 配送 / 取得用の `reqwest::Client` を組み立てる。
///
/// `AppState` から共有する想定なので、内部の `Arc<reqwest::Inner>` がそのまま
/// `Clone` で参照カウントされる。**1 プロセス 1 インスタンス**を維持し、
/// connection pool を再利用させる。
pub(crate) fn build_client() -> anyhow::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .redirect(Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // HTTP/2 ALPN を明示的に許可 (rustls-tls-native-roots は ALPN を
        // 設定するが、reqwest 側で http2_prior_knowledge() は不要。普通の
        // ALPN 交渉で h2 になればよく、相手が h1 なら自然に fallback する)。
        .http2_keep_alive_interval(Some(Duration::from_secs(20)))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest::Client for outbound delivery")
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 連合相手のアクセスログから判別できるよう `sakurasato/<version>` を載せる。
/// プロジェクト URL を `+(...)` 形式で添えるのは Mastodon / Misskey と同じ慣例。
const USER_AGENT: &str = concat!(
    "sakurasato/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/nananek/sakurasato)"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_succeeds() {
        // builder が固まる程度の smoke test。実 HTTP は撃たない。
        let _client = build_client().unwrap();
    }

    #[test]
    fn user_agent_includes_project_marker() {
        // 連合相手のログから sakurasato を判別できることを担保。
        assert!(USER_AGENT.starts_with("sakurasato/"));
        assert!(USER_AGENT.contains("github.com/nananek/sakurasato"));
    }
}
