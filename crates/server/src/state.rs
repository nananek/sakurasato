use std::sync::Arc;

use anyhow::Context;
use sakurasato_core::Config;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

#[derive(Clone, Debug)]
pub struct AppState(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    config: Config,
    pool: PgPool,
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
        Ok(Self::from_pool(pool, config))
    }

    /// Build the state from an already-prepared pool. Used by integration
    /// tests where `#[sqlx::test]` supplies a per-test pool.
    pub fn from_pool(pool: PgPool, config: Config) -> Self {
        Self(Arc::new(Inner { config, pool }))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    /// Compute the canonical AP actor `id` URI for `username` against the
    /// configured public host.
    pub fn local_actor_ap_id(&self, username: &str) -> String {
        format!("https://{}/users/{username}", self.0.config.server.host)
    }
}
