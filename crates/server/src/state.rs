use std::sync::Arc;

use anyhow::Context;
use reqwest::Client;
use sakurasato_core::Config;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::http_client;

#[derive(Clone, Debug)]
pub struct AppState(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    config: Config,
    pool: PgPool,
    http: Client,
    /// SSRF ガード (`delivery::inbox_host_blocked`) を緩めるかどうか。
    ///
    /// **本番経路 [`AppState::from_config`] は常に `false`** ── 配送ワーカが
    /// `http://127.0.0.1/admin` のような内部宛先に POST するのを遮断する。
    /// **テスト経路 [`AppState::from_pool`] のみ `true`** ── 統合テストは
    /// `127.0.0.1:0` の axum サーバを立ててダミー inbox にするため、
    /// loopback を許可しないとテスト不能。本番 `from_config` を通る限り
    /// 常に false 固定なので、CLI / serve 経路で内部宛先が通る経路は無い。
    allow_internal_inbox: bool,
}

impl AppState {
    /// Build the `AppState` by resolving the DB URL (with password file
    /// substitution) and connecting to Postgres.
    pub async fn from_config(config: Config) -> anyhow::Result<Self> {
        let url = config
            .database
            .resolved_url()
            .context("resolve database URL")?;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .context("connect to PostgreSQL")?;
        let http = http_client::build_client()?;
        Ok(Self(Arc::new(Inner {
            config,
            pool,
            http,
            allow_internal_inbox: false,
        })))
    }

    /// Build the state from an already-prepared pool. Used by integration
    /// tests where `#[sqlx::test]` supplies a per-test pool.
    ///
    /// SSRF ガードを緩める ([`Inner::allow_internal_inbox`] = `true`) ─
    /// 統合テスト用 inbox を `127.0.0.1` で立てるため。本番経路には影響しない。
    pub fn from_pool(pool: PgPool, config: Config) -> Self {
        let http = http_client::build_client().expect("reqwest builder is infallible in tests");
        Self(Arc::new(Inner {
            config,
            pool,
            http,
            allow_internal_inbox: true,
        }))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    /// Shared outbound HTTP client. Cloning is cheap (the inner state is
    /// `Arc`-wrapped by `reqwest` itself), so callers may clone freely if
    /// they need to spawn detached delivery tasks.
    pub fn http_client(&self) -> &Client {
        &self.0.http
    }

    /// SSRF ガードを緩めるか。本番 (`from_config`) は常に `false`。
    pub(crate) fn allow_internal_inbox(&self) -> bool {
        self.0.allow_internal_inbox
    }

    /// Compute the canonical AP actor `id` URI for `username` against the
    /// configured public host.
    pub fn local_actor_ap_id(&self, username: &str) -> String {
        format!("https://{}/users/{username}", self.0.config.server.host)
    }
}
