use std::sync::Arc;

use anyhow::Context;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use reqwest::Client;
use sakurasato_core::Config;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;

use crate::http_client;
use crate::local_api::stream::{TIMELINE_CHANNEL_CAPACITY, TimelineEvent};

#[derive(Clone, Debug)]
pub struct AppState(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    config: Config,
    pool: PgPool,
    http: Client,
    /// SSE (`GET /api/v1/stream`) を購読しているクライアントに新規 Note 等を
    /// 配るための tokio broadcast channel。受信者ごとに `Sender::subscribe()`
    /// で `Receiver` を取り、ハンドラが axum SSE Event に変換して流す。
    ///
    /// capacity を超えるとラギング (`RecvError::Lagged`) で古い順に drop
    /// される ── SSE 接続が一時的に詰まっても publisher (POST notes 等) は
    /// ブロックしない設計。capacity は [`TIMELINE_CHANNEL_CAPACITY`] を参照。
    timeline_tx: broadcast::Sender<TimelineEvent>,
    /// versitygw (S3 互換) 向けクライアント。`from_config` は config から
    /// `access_key` / `secret_access_key` / endpoint / region を読んで構築し、
    /// `from_pool` (テスト) はダミー endpoint で構築する ── 本物の S3 に
    /// 出ていかない契約。`force_path_style` = true は versitygw が path-style
    /// (`/<bucket>/<key>`) しか受けないため。
    s3: S3Client,
    /// SSRF ガード ([`crate::net_guard::host_blocked`]) を緩めるかどうか。
    ///
    /// **本番経路 [`AppState::from_config`] は常に `false`** ── 配送ワーカが
    /// `http://127.0.0.1/admin` のような内部宛先に POST するのを遮断する。
    /// **テスト経路 [`AppState::from_pool`] のみ `true`** ── 統合テストは
    /// `127.0.0.1:0` の axum サーバを立ててダミー inbox にするため、
    /// loopback を許可しないとテスト不能。本番 `from_config` を通る限り
    /// 常に false 固定なので、CLI / serve 経路で内部宛先が通る経路は無い。
    allow_internal_inbox: bool,
    /// 未知 actor 到来時に remote から actor JSON を fetch するかどうか。
    ///
    /// **本番経路 `from_config` は `true`** ── 受信 inbox で未知 keyId が
    /// 来たら CLAUDE.md §3 暫定で server 直 fetch する。
    /// **テスト経路 `from_pool` は `false`** ── 統合テストで実 DNS / 実
    /// ネットワークに到達しないようにする。テストは必要な actor を予め
    /// `repo::actor::insert` で seed しておく契約。
    enable_remote_fetch: bool,
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
        let s3 = build_s3_client(&config)?;
        let (timeline_tx, _) = broadcast::channel(TIMELINE_CHANNEL_CAPACITY);
        Ok(Self(Arc::new(Inner {
            config,
            pool,
            http,
            s3,
            timeline_tx,
            allow_internal_inbox: false,
            enable_remote_fetch: true,
        })))
    }

    /// Build the state from an already-prepared pool. Used by integration
    /// tests where `#[sqlx::test]` supplies a per-test pool.
    ///
    /// SSRF ガードを緩める ([`Inner::allow_internal_inbox`] = `true`) ─
    /// 統合テスト用 inbox を `127.0.0.1` で立てるため。本番経路には影響しない。
    pub fn from_pool(pool: PgPool, config: Config) -> Self {
        let http = http_client::build_client().expect("reqwest builder is infallible in tests");
        // テスト経路は実 S3 / versitygw に出ない契約。ダミー endpoint で構築。
        // GET /media/<key> を叩くテストはコネクション失敗で 500 を返すだけ。
        let s3 =
            build_s3_client(&config).expect("aws-sdk-s3 builder is infallible from static creds");
        let (timeline_tx, _) = broadcast::channel(TIMELINE_CHANNEL_CAPACITY);
        Self(Arc::new(Inner {
            config,
            pool,
            http,
            s3,
            timeline_tx,
            allow_internal_inbox: true,
            enable_remote_fetch: false,
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

    /// 未知 actor 到来時に remote fetch を試みるか。本番 (`from_config`)
    /// は `true`、テスト (`from_pool`) は `false`。
    pub(crate) fn enable_remote_fetch(&self) -> bool {
        self.0.enable_remote_fetch
    }

    /// Compute the canonical AP actor `id` URI for `username` against the
    /// configured public host.
    pub fn local_actor_ap_id(&self, username: &str) -> String {
        format!("https://{}/users/{username}", self.0.config.server.host)
    }

    /// S3 client targeting versitygw. Read-only operations (M4 PR1 = GET
    /// /media/{key}); writes arrive with media uploads in M4 PR2 / M7.
    pub fn s3_client(&self) -> &S3Client {
        &self.0.s3
    }

    /// SSE 配信用 broadcast sender。POST notes / 受信 Note dispatch から
    /// `send(event)` を呼び、`GET /api/v1/stream` ハンドラが
    /// [`broadcast::Sender::subscribe`] で `Receiver` を取って消費する。
    ///
    /// 受信者が居ない状態で送ると [`broadcast::Sender::send`] は `Err` を返すが、
    /// publisher 側は気にせず無視する設計 (SSE を誰も購読していないのは正常)。
    pub fn timeline_sender(&self) -> &broadcast::Sender<TimelineEvent> {
        &self.0.timeline_tx
    }
}

/// Build an aws-sdk-s3 [`Client`](S3Client) from [`StorageConfig`](sakurasato_core::config::StorageConfig).
///
/// - **Static credentials** from `access_key_id` + `resolved_secret_access_key`
///   (file-backed when configured). No env / credential file lookups so the
///   server doesn't accidentally pick up an unrelated `~/.aws/credentials`.
/// - **Endpoint override** to point at versitygw (typically `http://versitygw:7070`).
/// - **`force_path_style = true`** — versitygw serves `/<bucket>/<key>` only,
///   not the virtual-hosted `<bucket>.s3....` form.
/// - **`behavior_version_latest`** — required by aws-sdk-s3 1.x.
fn build_s3_client(config: &Config) -> anyhow::Result<S3Client> {
    let secret = config
        .storage
        .resolved_secret_access_key()
        .context("resolve storage secret_access_key")?;
    let creds = Credentials::new(
        config.storage.access_key_id.clone(),
        secret,
        None,
        None,
        "sakurasato-config",
    );
    let s3_conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(config.storage.region.clone()))
        .endpoint_url(config.storage.endpoint.clone())
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    Ok(S3Client::from_conf(s3_conf))
}
