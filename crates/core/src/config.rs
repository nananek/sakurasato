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
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};

/// URL userinfo encode set per RFC 3986: percent-encode anything outside
/// the unreserved set so passwords with `@`/`:`/`/`/`?`/`#`/`%` don't
/// break the parser.
const USERINFO_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

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
    /// `PostgreSQL` connection URL. May contain the literal placeholder
    /// `{password}` which is substituted at runtime with the contents of
    /// [`Self::password_file`].
    pub url: String,
    /// Optional path to a file containing the database password. When set,
    /// its contents (with surrounding whitespace trimmed) replace
    /// `{password}` in [`Self::url`]. Required when the URL contains the
    /// placeholder.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
}

impl DatabaseConfig {
    /// Resolve the connection URL, substituting `{password}` from
    /// [`Self::password_file`] when present. The password is percent-encoded
    /// per RFC 3986 userinfo before insertion, so passwords containing
    /// `@`, `:`, `/`, `#`, or other reserved characters do not corrupt
    /// the URL.
    pub fn resolved_url(&self) -> anyhow::Result<String> {
        if !self.url.contains("{password}") {
            return Ok(self.url.clone());
        }
        let path = self.password_file.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "database.url contains {{password}} placeholder but database.password_file is unset"
            )
        })?;
        let raw = std::fs::read_to_string(path).map_err(|err| {
            anyhow::anyhow!(
                "failed to read database password_file {}: {err}",
                path.display()
            )
        })?;
        let encoded = utf8_percent_encode(raw.trim(), USERINFO_ENCODE).to_string();
        Ok(self.url.replace("{password}", &encoded))
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// S3 endpoint (versitygw is reached internally, never published).
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    /// S3 secret access key. Skipped from serialization to avoid leaks via
    /// `serde_json::to_string(&config)` and custom-redacted in `Debug` so
    /// it never appears in `tracing::debug!(?config)` either. Prefer
    /// setting [`Self::secret_access_key_file`] over hard-coding this.
    #[serde(skip_serializing)]
    pub secret_access_key: String,
    /// Optional path to a file containing the S3 secret access key. When
    /// set, its contents (trimmed) take precedence over
    /// [`Self::secret_access_key`].
    #[serde(default)]
    pub secret_access_key_file: Option<PathBuf>,
}

impl StorageConfig {
    /// Returns the effective secret access key, reading from
    /// [`Self::secret_access_key_file`] when set.
    pub fn resolved_secret_access_key(&self) -> anyhow::Result<String> {
        match &self.secret_access_key_file {
            Some(path) => {
                let raw = std::fs::read_to_string(path).map_err(|err| {
                    anyhow::anyhow!(
                        "failed to read storage secret_access_key_file {}: {err}",
                        path.display()
                    )
                })?;
                Ok(raw.trim().to_owned())
            }
            None => Ok(self.secret_access_key.clone()),
        }
    }
}

impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("secret_access_key_file", &self.secret_access_key_file)
            .finish()
    }
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
url = "postgres://sakurasato:{password}@postgres:5432/sakurasato"

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
                "postgres://sakurasato:{password}@postgres:5432/sakurasato"
            );
            Ok(())
        });
    }

    #[test]
    fn resolved_url_returns_as_is_without_placeholder() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_DATABASE__URL",
                "postgres://u:p@postgres:5432/sakurasato",
            );
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(
                cfg.database.resolved_url().unwrap(),
                "postgres://u:p@postgres:5432/sakurasato"
            );
            Ok(())
        });
    }

    #[test]
    fn resolved_url_substitutes_password_from_file() {
        Jail::expect_with(|jail| {
            let dir = jail.directory().to_path_buf();
            let pw_path = dir.join("pw.txt");
            std::fs::write(&pw_path, "s3cret\n").unwrap();
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_DATABASE__PASSWORD_FILE",
                pw_path.to_str().unwrap(),
            );
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(
                cfg.database.resolved_url().unwrap(),
                "postgres://sakurasato:s3cret@postgres:5432/sakurasato"
            );
            Ok(())
        });
    }

    #[test]
    fn resolved_url_percent_encodes_special_chars() {
        Jail::expect_with(|jail| {
            let dir = jail.directory().to_path_buf();
            let pw_path = dir.join("pw.txt");
            // 全部生のまま埋めると URL の userinfo 区切り (`@`, `:`) を壊す。
            std::fs::write(&pw_path, "p@ss/wo:rd#1\n").unwrap();
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_DATABASE__PASSWORD_FILE",
                pw_path.to_str().unwrap(),
            );
            let cfg = Config::load(&path, None).unwrap();
            let resolved = cfg.database.resolved_url().unwrap();
            assert_eq!(
                resolved,
                "postgres://sakurasato:p%40ss%2Fwo%3Ard%231@postgres:5432/sakurasato"
            );
            // url クレートで再パースして host が壊れていないことも確認。
            let parsed = url::Url::parse(&resolved).unwrap();
            assert_eq!(parsed.host_str(), Some("postgres"));
            assert_eq!(parsed.password(), Some("p%40ss%2Fwo%3Ard%231"));
            Ok(())
        });
    }

    #[test]
    fn resolved_secret_access_key_reads_file_when_set() {
        Jail::expect_with(|jail| {
            let dir = jail.directory().to_path_buf();
            let key_path = dir.join("s3.txt");
            std::fs::write(&key_path, "the-real-key\n").unwrap();
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_STORAGE__SECRET_ACCESS_KEY_FILE",
                key_path.to_str().unwrap(),
            );
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(
                cfg.storage.resolved_secret_access_key().unwrap(),
                "the-real-key"
            );
            Ok(())
        });
    }

    #[test]
    fn storage_debug_redacts_secret_access_key() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            let cfg = Config::load(&path, None).unwrap();
            let dbg = format!("{:?}", cfg.storage);
            assert!(dbg.contains("<redacted>"), "debug must mask secret: {dbg}");
            assert!(
                !dbg.contains("minio12345"),
                "secret leaked into debug: {dbg}"
            );
            Ok(())
        });
    }

    #[test]
    fn storage_serialize_skips_secret_access_key() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            let cfg = Config::load(&path, None).unwrap();
            let json = serde_json::to_string(&cfg.storage).unwrap();
            assert!(
                !json.contains("minio12345"),
                "secret leaked into JSON: {json}"
            );
            assert!(!json.contains("secret_access_key\":\""), "{json}");
            Ok(())
        });
    }

    #[test]
    fn resolved_url_errors_when_placeholder_unmet() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            let cfg = Config::load(&path, None).unwrap();
            assert!(cfg.database.resolved_url().is_err());
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
