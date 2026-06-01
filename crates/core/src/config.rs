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
    /// HTTP bind address inside the container (TCP).
    ///
    /// **Deprecated**: prefer [`Self::public_listen`] which accepts a
    /// `tcp://host:port` or `unix:/path` URI. This field is kept as a
    /// fallback when `public_listen` is unset so existing deployments
    /// continue to work unchanged.
    pub bind: String,
    /// Path of the Unix domain socket exposed to the TUI client on the host.
    ///
    /// **Deprecated**: prefer [`Self::local_api_listen`] which accepts a
    /// `unix:/path` or `tcp://host:port` URI. Kept as a fallback for
    /// existing deployments.
    pub local_api_socket: PathBuf,
    /// Optional URI for the **public AP listener**. When set, overrides
    /// [`Self::bind`]. Accepts:
    /// - `tcp://host:port` (= `0.0.0.0:443` 形式と等価)
    /// - `unix:/path` または `unix:///path` (= Cloudflared 等の UDS origin 用)
    ///
    /// 推奨運用: Cloudflared Tunnel を使う場合は `unix:/run/...` に倒し、
    /// host にポートを露出しない。
    #[serde(default)]
    pub public_listen: Option<String>,
    /// Optional URI for the **local API (`/api/v1/*`) listener**. When set,
    /// overrides [`Self::local_api_socket`]. Accepts the same scheme as
    /// [`Self::public_listen`].
    ///
    /// 推奨運用: Tailscale tailnet 経由で TUI を別端末から触る場合は
    /// `tcp://0.0.0.0:18080` 等に倒す ── tailscale は TCP/UDP しか流せ
    /// ないため、UDS のままだと `tailscale serve` の薄いブリッジが要る。
    /// 直接ホスト上で TUI を動かすなら従来どおり `unix:/run/...` でよい。
    #[serde(default)]
    pub local_api_listen: Option<String>,
    /// Single user actor handle (the only local user).
    pub user: String,
    /// 任意のサーバ運営情報 (description / 管理者連絡先 / テーマ色等)。
    /// `NodeInfo` の `metadata` に反映され、他鯖の探索系で表示される。
    /// お一人様サーバなのでデフォルトは全フィールド `None` (= 何も出さない)。
    #[serde(default)]
    pub info: ServerInfo,
}

/// `NodeInfo.metadata` 経由で他鯖に公開するサーバ情報。
///
/// `config.toml` の `[server.info]` セクションで自由に書ける:
///
/// ```toml
/// [server.info]
/// name = "My Quiet Sakurasato"
/// description = "neko の隠れ家。Mastodon / Misskey 連合。"
/// admin_name = "neko"
/// admin_contact = "mailto:neko@example.com"
/// theme_color = "#ffb7c5"
/// banner_url = "https://example.com/banner.webp"
/// ```
///
/// すべて optional。空値は emit しない (= 他鯖が "null" を変な値と見なすのを防ぐ)。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ServerInfo {
    /// 表示用サーバ名 (Mastodon の "Instance name" 相当)。未設定なら host を使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// サーバ説明文。Markdown は許可しないテキスト前提。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 管理者の表示名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_name: Option<String>,
    /// 管理者連絡先 (`mailto:` URI 推奨)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_contact: Option<String>,
    /// テーマ色 (`#rrggbb`)。Mastodon / Misskey の UI ハイライトに使われる慣習。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_color: Option<String>,
    /// バナー画像 URL (= サーバ紹介ページ用の横長画像)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_url: Option<String>,
    /// `ToS` / プライバシーポリシー URL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_url: Option<String>,
    /// ソフトウェアリポジトリ (デフォルトは sakurasato 本家 ── fork した時の上書き用)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
}

/// 抽象 listener (TCP / Unix domain socket)。`server` 側で `axum::serve` の
/// バインド先として、`tui` 側で接続先として使う。
///
/// URI 表現:
/// - `tcp://host:port` (`host` は `0.0.0.0` / `127.0.0.1` / `[::]` 等)
/// - `unix:/abs/path` または `unix:///abs/path`
///   (`unix://localhost/abs/path` も同義扱い)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// TCP listener。`host:port` 文字列をそのまま保持する (= `TcpListener::bind` 形式)。
    Tcp(String),
    /// Unix domain socket。絶対パス推奨。
    Unix(PathBuf),
}

impl Listen {
    /// 文字列 URI から `Listen` を組む。許容: `tcp://host:port` / `unix:/path` /
    /// `unix:///path`。スキーム無しは原則拒否 (= 設定の事故を防ぐ)。
    ///
    /// `unix://host/path` 形式は **host が `localhost` または空の場合のみ**
    /// 受理する。それ以外の host は「abstract namespace を意図したのか
    /// remote unix socket か」が曖昧なので 400 で弾く。
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            anyhow::bail!("listen URI is empty");
        }
        if let Some(rest) = trimmed.strip_prefix("tcp://") {
            if rest.is_empty() {
                anyhow::bail!("tcp:// URI must have host:port body");
            }
            // **[PR #70 round-2 #1]**: `rest.contains(':')` だけでは IPv6 bare
            // address (= `[::1]`) が `:` を含むため port 欠落をすり抜ける。
            // `std::net::SocketAddr::from_str` で構造検証する ── これで
            // `0.0.0.0:8080` / `127.0.0.1:18080` / `[::1]:443` / `[::]:8080`
            // は通り、`[::1]` (port 欠落) / `localhost:abc` (port 非数値) /
            // `localhost:8080` (DNS 必要、bind 時の errno 任せ) は事前に弾く。
            //
            // ただし `localhost:8080` のような DNS ホスト名形式は `SocketAddr`
            // が受け付けない ── 実装の互換性のため「`SocketAddr` で通れば OK、
            // 通らなければ `host:port` パターン (= 末尾が `:digits`) で fall
            // back 検証」の 2 段で受ける。
            if rest.parse::<std::net::SocketAddr>().is_err() {
                // IPv6 ブラケットの外で最後の `:` を探し、その後が数値なら OK。
                let bracket_close = rest.rfind(']');
                let last_colon = rest.rfind(':');
                let port_str = match (bracket_close, last_colon) {
                    // `[::1]:8080` パターンは SocketAddr 経路で通っているはず
                    // なので、ここに来るのは異常 (= `[::1]` ポート無し等)。
                    (Some(close), Some(colon)) if colon > close => &rest[colon + 1..],
                    // ブラケット無しなら最後の `:` 以降が port (= `host:port`)。
                    (None, Some(colon)) => &rest[colon + 1..],
                    _ => {
                        anyhow::bail!("tcp:// URI must include :port (got {rest:?})");
                    }
                };
                if port_str.is_empty() || !port_str.chars().all(|c| c.is_ascii_digit()) {
                    anyhow::bail!("tcp:// URI port must be numeric (got {rest:?})");
                }
                // port は 0..=65535 (u16 範囲)。port 0 は OS 任せの自動割当
                // 慣習があるので受理する。
                let port: u32 = port_str
                    .parse()
                    .map_err(|_| anyhow::anyhow!("tcp:// port {port_str:?} is not numeric"))?;
                if port > 65535 {
                    anyhow::bail!("tcp:// port must be 0..=65535 (got {port})");
                }
            }
            return Ok(Self::Tcp(rest.to_string()));
        }
        if let Some(rest) = trimmed.strip_prefix("unix://") {
            // unix:///path or unix://localhost/path
            // **[PR #70 round-2 #4]**: `unix:////path` (= `///path`) は
            // `strip_prefix('/')` 後に `//path` が残るので二重スラッシュ検出で弾く。
            let path_str = if let Some(p) = rest.strip_prefix('/') {
                // `unix:///path` (= host 部空) → `/path`
                if p.starts_with('/') {
                    anyhow::bail!(
                        "unix:// URI must not have extra leading slashes (got {trimmed:?})"
                    );
                }
                format!("/{p}")
            } else if let Some(p) = rest.strip_prefix("localhost/") {
                if p.starts_with('/') {
                    anyhow::bail!(
                        "unix://localhost/ URI must not have extra leading slashes (got {trimmed:?})"
                    );
                }
                format!("/{p}")
            } else {
                anyhow::bail!(
                    "unix:// URI must be `unix:/path`, `unix:///path`, or `unix://localhost/path` (got {trimmed:?})"
                );
            };
            if path_str == "/" || path_str.is_empty() {
                anyhow::bail!("unix:// URI must have a non-empty path");
            }
            return Ok(Self::Unix(PathBuf::from(path_str)));
        }
        if let Some(rest) = trimmed.strip_prefix("unix:") {
            // unix:/path
            if rest.is_empty() || !rest.starts_with('/') {
                anyhow::bail!("unix: URI must have an absolute path (got {trimmed:?})");
            }
            // `unix://...` は上で先に処理済みなので、ここは `unix:/...` のみ。
            // `unix://` は通常 `strip_prefix("unix://")` で先に取られるが、
            // 念のため再確認 (= `unix:///` のような形が万一来ても上で弾かれる)。
            return Ok(Self::Unix(PathBuf::from(rest)));
        }
        anyhow::bail!("listen URI must start with `tcp://` or `unix:` (got {trimmed:?})")
    }

    /// 診断ログ・エラーメッセージ向けの表示文字列 (= 入力 URI を可逆に復元)。
    pub fn display(&self) -> String {
        match self {
            Self::Tcp(s) => format!("tcp://{s}"),
            Self::Unix(p) => format!("unix:{}", p.display()),
        }
    }
}

impl ServerConfig {
    /// 公開 AP listener の有効値。`public_listen` 優先、無ければ `bind` を
    /// TCP として fall back する。
    pub fn public_listener(&self) -> anyhow::Result<Listen> {
        if let Some(uri) = self.public_listen.as_deref() {
            Listen::parse(uri).map_err(|e| anyhow::anyhow!("server.public_listen invalid: {e}"))
        } else {
            Ok(Listen::Tcp(self.bind.clone()))
        }
    }

    /// ローカル API listener の有効値。`local_api_listen` 優先、無ければ
    /// `local_api_socket` を UDS として fall back する。
    pub fn local_api_listener(&self) -> anyhow::Result<Listen> {
        if let Some(uri) = self.local_api_listen.as_deref() {
            Listen::parse(uri).map_err(|e| anyhow::anyhow!("server.local_api_listen invalid: {e}"))
        } else {
            Ok(Listen::Unix(self.local_api_socket.clone()))
        }
    }
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

/// `sqlx::postgres` が解釈しない URL クエリパラメタ。
/// 一致するパラメタは [`DatabaseConfig::resolved_url`] が捨てる。
///
/// **`channel_binding`** (Neon): TLS channel binding 強制フラグ。
/// libpq / psycopg などは認識するが sqlx 0.9 は未対応で、起動時に
/// `unknown URL parameter: channel_binding` の WARN を吐く (Issue #74)。
/// channel binding は Neon 側で TLS 終端時に強制されており、クライアントの
/// 同意フラグは advisory に過ぎないので、ここで剥がしても接続安全性に影響
/// しない (= 結局 server 側で `SCRAM-SHA-256-PLUS` を要求される)。
///
/// 将来 sqlx 側がサポートしたらこの allow-list から外す。
const SQLX_UNKNOWN_QUERY_PARAMS: &[&str] = &["channel_binding"];

impl DatabaseConfig {
    /// Resolve the connection URL, substituting `{password}` from
    /// [`Self::password_file`] when present. The password is percent-encoded
    /// per RFC 3986 userinfo before insertion, so passwords containing
    /// `@`, `:`, `/`, `#`, or other reserved characters do not corrupt
    /// the URL.
    ///
    /// **Issue #74**: 解決後の URL から sqlx が認識しないクエリパラメタ
    /// (= [`SQLX_UNKNOWN_QUERY_PARAMS`]) を取り除く。Neon の
    /// `channel_binding=require` で起動毎に WARN が出るのを抑止する。
    pub fn resolved_url(&self) -> anyhow::Result<String> {
        let substituted = if self.url.contains("{password}") {
            let path = self.password_file.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "database.url contains {{password}} placeholder but database.password_file is unset",
                )
            })?;
            let raw = std::fs::read_to_string(path).map_err(|err| {
                anyhow::anyhow!(
                    "failed to read database password_file {}: {err}",
                    path.display()
                )
            })?;
            let encoded = utf8_percent_encode(raw.trim(), USERINFO_ENCODE).to_string();
            self.url.replace("{password}", &encoded)
        } else {
            self.url.clone()
        };
        Ok(strip_unknown_query_params(
            &substituted,
            SQLX_UNKNOWN_QUERY_PARAMS,
        ))
    }
}

/// `url` の query string から `drop_keys` に一致するペアだけを除去する。
/// パースに失敗 (= 無効な URL) なら元の文字列を返す ── sqlx 側で
/// 接続時に同じパース失敗が再現するので、エラー報告は connect 側に任せる。
fn strip_unknown_query_params(url: &str, drop_keys: &[&str]) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !drop_keys.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.len() == parsed.query_pairs().count() {
        return parsed.into();
    }
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        let mut serializer = parsed.query_pairs_mut();
        serializer.clear();
        for (k, v) in &kept {
            serializer.append_pair(k, v);
        }
        // serializer は Drop 時に書き戻すので明示 drop して parsed を return。
        drop(serializer);
    }
    parsed.into()
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

    /// Issue #74: Neon の `channel_binding=require` クエリパラメタは sqlx 0.9
    /// が認識しないので `unknown URL parameter` の WARN を吐く。`resolved_url`
    /// が剥がして返すことを確認する。
    #[test]
    fn resolved_url_strips_channel_binding_query_param() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_DATABASE__URL",
                "postgres://u:p@neon.example/dbname?sslmode=require&channel_binding=require",
            );
            let cfg = Config::load(&path, None).unwrap();
            let resolved = cfg.database.resolved_url().unwrap();
            assert!(
                !resolved.contains("channel_binding"),
                "channel_binding must be stripped, got {resolved}"
            );
            assert!(
                resolved.contains("sslmode=require"),
                "sslmode must be kept, got {resolved}"
            );
            Ok(())
        });
    }

    #[test]
    fn resolved_url_preserves_url_when_only_unknown_param() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_DATABASE__URL",
                "postgres://u:p@neon.example/dbname?channel_binding=require",
            );
            let cfg = Config::load(&path, None).unwrap();
            let resolved = cfg.database.resolved_url().unwrap();
            assert_eq!(resolved, "postgres://u:p@neon.example/dbname");
            Ok(())
        });
    }

    #[test]
    fn strip_unknown_query_params_is_noop_when_no_match() {
        let url = "postgres://u:p@host:5432/db?sslmode=require";
        let cleaned = strip_unknown_query_params(url, &["channel_binding"]);
        assert_eq!(cleaned, url);
    }

    #[test]
    fn strip_unknown_query_params_returns_input_when_invalid_url() {
        // sqlx 側で同じパース失敗が再現するので、ここは noop で OK。
        let url = "not a url";
        let cleaned = strip_unknown_query_params(url, &["channel_binding"]);
        assert_eq!(cleaned, url);
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
    fn listen_parse_tcp_forms() {
        assert_eq!(
            Listen::parse("tcp://0.0.0.0:8080").unwrap(),
            Listen::Tcp("0.0.0.0:8080".into())
        );
        assert_eq!(
            Listen::parse("tcp://127.0.0.1:18080").unwrap(),
            Listen::Tcp("127.0.0.1:18080".into())
        );
        assert_eq!(
            Listen::parse("tcp://[::]:443").unwrap(),
            Listen::Tcp("[::]:443".into())
        );
    }

    #[test]
    fn listen_parse_unix_forms() {
        // unix:/path
        assert_eq!(
            Listen::parse("unix:/run/sakurasato/local.sock").unwrap(),
            Listen::Unix(PathBuf::from("/run/sakurasato/local.sock"))
        );
        // unix:///path
        assert_eq!(
            Listen::parse("unix:///run/sakurasato/local.sock").unwrap(),
            Listen::Unix(PathBuf::from("/run/sakurasato/local.sock"))
        );
        // unix://localhost/path
        assert_eq!(
            Listen::parse("unix://localhost/run/local.sock").unwrap(),
            Listen::Unix(PathBuf::from("/run/local.sock"))
        );
    }

    #[test]
    fn listen_parse_rejects_malformed() {
        // 空
        assert!(Listen::parse("").is_err());
        assert!(Listen::parse("   ").is_err());
        // スキーム無し
        assert!(Listen::parse("0.0.0.0:8080").is_err());
        assert!(Listen::parse("/run/local.sock").is_err());
        // tcp 不完全
        assert!(Listen::parse("tcp://").is_err());
        assert!(Listen::parse("tcp://localhost").is_err()); // :port 欠落
        // unix 不完全
        assert!(Listen::parse("unix:").is_err());
        assert!(Listen::parse("unix:relative").is_err()); // 相対パス
        assert!(Listen::parse("unix:///").is_err()); // 空パス
        // unix://host/path で host が localhost でない (= remote unix の曖昧さ)
        assert!(Listen::parse("unix://otherhost/path").is_err());
        // 未対応スキーム
        assert!(Listen::parse("http://localhost/").is_err());
    }

    /// **[PR #70 round-2 #1]**: IPv6 bare アドレス (`tcp://[::1]`) は `::`
    /// で `:` 含有チェックを通過する旧バグの回帰テスト。
    #[test]
    fn listen_parse_rejects_ipv6_without_port() {
        assert!(Listen::parse("tcp://[::1]").is_err());
        assert!(Listen::parse("tcp://[::]").is_err());
        assert!(Listen::parse("tcp://[2001:db8::1]").is_err());
        // port 部が数値でない
        assert!(Listen::parse("tcp://[::1]:abc").is_err());
        assert!(Listen::parse("tcp://127.0.0.1:abc").is_err());
        // port が範囲外。port 0 は **listener bind では OS が空きを割当てる
        // 慣習** なので受理する (= テストでも `127.0.0.1:0` が頻出)。
        assert!(Listen::parse("tcp://localhost:65536").is_err());
    }

    /// **[PR #70 round-2 #1]**: IPv6 with port は通す。
    #[test]
    fn listen_parse_accepts_ipv6_with_port() {
        assert_eq!(
            Listen::parse("tcp://[::1]:443").unwrap(),
            Listen::Tcp("[::1]:443".into())
        );
        assert_eq!(
            Listen::parse("tcp://[::]:8080").unwrap(),
            Listen::Tcp("[::]:8080".into())
        );
    }

    /// **[PR #70 round-2 #1]**: hostname 形式 (= `localhost:port`) も実用上は
    /// 受け入れる ── `SocketAddr` parse は通らないが、末尾 `:数値` で fall back。
    #[test]
    fn listen_parse_accepts_hostname_with_port() {
        assert_eq!(
            Listen::parse("tcp://localhost:8080").unwrap(),
            Listen::Tcp("localhost:8080".into())
        );
        assert_eq!(
            Listen::parse("tcp://example.test:443").unwrap(),
            Listen::Tcp("example.test:443".into())
        );
    }

    /// **[PR #70 round-2 #4]**: `unix:////path` (= スラッシュ 4 つ以上) は弾く。
    #[test]
    fn listen_parse_rejects_extra_leading_slashes_in_unix() {
        assert!(Listen::parse("unix:////run/local.sock").is_err());
        assert!(Listen::parse("unix://localhost//run/local.sock").is_err());
    }

    #[test]
    fn server_config_listener_resolution() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            // 既定: public_listen / local_api_listen 未設定 → bind / local_api_socket fallback。
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(
                cfg.server.public_listener().unwrap(),
                Listen::Tcp("0.0.0.0:8080".into())
            );
            assert_eq!(
                cfg.server.local_api_listener().unwrap(),
                Listen::Unix(PathBuf::from("/run/sakurasato/local.sock"))
            );
            Ok(())
        });
    }

    #[test]
    fn server_config_listener_override_via_env() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env(
                "SAKURASATO_SERVER__PUBLIC_LISTEN",
                "unix:/run/sakurasato/public.sock",
            );
            jail.set_env("SAKURASATO_SERVER__LOCAL_API_LISTEN", "tcp://0.0.0.0:18080");
            let cfg = Config::load(&path, None).unwrap();
            assert_eq!(
                cfg.server.public_listener().unwrap(),
                Listen::Unix(PathBuf::from("/run/sakurasato/public.sock"))
            );
            assert_eq!(
                cfg.server.local_api_listener().unwrap(),
                Listen::Tcp("0.0.0.0:18080".into())
            );
            Ok(())
        });
    }

    #[test]
    fn server_config_invalid_listen_uri_surfaces() {
        Jail::expect_with(|jail| {
            let path = write_default(jail);
            jail.set_env("SAKURASATO_SERVER__PUBLIC_LISTEN", "garbage://nope");
            let cfg = Config::load(&path, None).unwrap();
            let err = cfg.server.public_listener().unwrap_err().to_string();
            assert!(
                err.contains("public_listen invalid"),
                "expected wrapped error, got {err}",
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
