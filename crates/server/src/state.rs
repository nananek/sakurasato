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
        Ok(Self(Arc::new(Inner { config, pool, http })))
    }

    /// Build the state from an already-prepared pool. Used by integration
    /// tests where `#[sqlx::test]` supplies a per-test pool.
    pub fn from_pool(pool: PgPool, config: Config) -> Self {
        // テストでも reqwest::Client は同等の builder で組む。テストは外向き
        // HTTP を撃たない (`mockito` か `#[ignore]` で隔離) ので Client が
        // ホスト DNS を引いてしまうことは無い。
        let http = http_client::build_client().expect("reqwest builder is infallible in tests");
        Self(Arc::new(Inner { config, pool, http }))
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

    /// Compute the canonical AP actor `id` URI for `username` against the
    /// configured public host.
    pub fn local_actor_ap_id(&self, username: &str) -> String {
        format!("https://{}/users/{username}", self.0.config.server.host)
    }
}
