//! [`ProxyState`] — ルータが共有するランタイム状態。
//!
//! 主に `reqwest::Client` (外部 GET) と設定値 (`max_bytes`) を保持する。
//! `Arc<ProxyState>` を axum の State として渡す。

use std::sync::Arc;

use reqwest::Client;
use sakurasato_core::Config;

use crate::http_client;

/// media-proxy の共有状態。`Arc` で包んで axum の `State` に載せる。
#[derive(Debug)]
pub struct ProxyState {
    config: Config,
    http: Client,
}

impl ProxyState {
    /// 本番経路: `Config` から `reqwest::Client` を構築。
    pub fn from_config(config: Config) -> anyhow::Result<Arc<Self>> {
        let http = http_client::build_client()?;
        Ok(Arc::new(Self { config, http }))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// `max_bytes`: ダウンロード本体 / 受信本文の上限。`usize` に丸めて返す。
    /// 環境変数で 64bit の巨大値が入っていた場合は `usize::MAX` にクランプ。
    pub fn max_bytes(&self) -> usize {
        usize::try_from(self.config.media_proxy.max_bytes).unwrap_or(usize::MAX)
    }

    /// `max_pixels`: デコード時の `image::Limits` に渡す。
    pub fn max_pixels(&self) -> u64 {
        self.config.media_proxy.max_pixels
    }
}
