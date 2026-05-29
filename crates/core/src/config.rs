//! Configuration loader.
//!
//! Reads `config/default.toml`, then overlays any user-supplied TOML file,
//! then overlays environment variables prefixed with `SAKURASATO_`.
//! Nested keys use `__` as the separator (e.g. `SAKURASATO_DATABASE__URL`).

use std::path::{Path, PathBuf};

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Top-level Sakurasato configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub media_proxy: MediaProxyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    /// Public-facing hostname (used for actor IDs, `WebFinger`, etc.).
    pub host: String,
    /// HTTP bind address inside the container.
    pub bind: String,
    /// Path of the Unix domain socket exposed to the TUI client on the host.
    pub local_api_socket: PathBuf,
    /// Single user actor handle (the only local user).
    pub user: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    /// `PostgreSQL` connection URL.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// S3 endpoint (versitygw is reached internally, never published).
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaProxyConfig {
    /// Unix socket used by `server` to talk to `media-proxy`.
    pub socket: PathBuf,
    /// Maximum bytes accepted from a remote fetch.
    pub max_bytes: u64,
    /// Maximum pixels (width * height) for decoded images.
    pub max_pixels: u64,
}

impl Config {
    /// Load configuration from `config/default.toml` (next to the binary's
    /// working directory by default), an optional user override, and the
    /// environment.
    pub fn load(default_path: impl AsRef<Path>, overlay: Option<&Path>) -> anyhow::Result<Self> {
        let mut figment = Figment::new().merge(Toml::file(default_path.as_ref()));
        if let Some(path) = overlay {
            figment = figment.merge(Toml::file(path));
        }
        let figment = figment.merge(Env::prefixed("SAKURASATO_").split("__"));
        figment.extract::<Self>().map_err(Into::into)
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Error is upstream-sized; Jail returns it.
mod tests {
    use super::*;
    use figment::Jail;

    fn write_default(jail: &mut Jail) -> std::path::PathBuf {
        let path = jail.directory().join("default.toml");
        std::fs::write(
            &path,
            r#"
[server]
host = "example.test"
bind = "0.0.0.0:8080"
local_api_socket = "/run/sakurasato/local.sock"
user = "me"

[database]
url = "postgres://sakurasato@postgres:5432/sakurasato"

[storage]
endpoint = "http://versitygw:7070"
bucket = "sakurasato"
region = "us-east-1"
access_key_id = "minio"
secret_access_key = "minio12345"

[media_proxy]
socket = "/run/sakurasato/media.sock"
max_bytes = 26214400
max_pixels = 33554432
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn loads_defaults() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(cfg.server.host, "example.test");
            assert_eq!(
                cfg.database.url,
                "postgres://sakurasato@postgres:5432/sakurasato"
            );
            Ok(())
        });
    }

    #[test]
    fn env_overrides_nested_key() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env("SAKURASATO_SERVER__HOST", "override.test");
            jail.set_env("SAKURASATO_MEDIA_PROXY__MAX_BYTES", "1024");
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(cfg.server.host, "override.test");
            assert_eq!(cfg.media_proxy.max_bytes, 1024);
            Ok(())
        });
    }
}
