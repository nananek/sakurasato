//! [`ProxyState`] — ルータが共有するランタイム状態。
//!
//! 主に `reqwest::Client` (外部 GET) と設定値 (`max_bytes`) を保持する。
//! `Arc<ProxyState>` を axum の State として渡す。

use std::sync::Arc;

use reqwest::Client;
use sakurasato_core::Config;
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::http_client;

/// 同時に処理する fetch / sanitize ジョブ数の上限。///
/// 対象は `fetch` / `sanitize` (画像) + `video_sanitize` (動画) + 将来の
/// 重いジョブ全般。1 リクエストあたりのメモリは `MAX_ANIMATED_TOTAL_FRAME_BYTES` (128 MiB) +
/// デコード / エンコードの一時バッファ、動画は入力 + コピーで約 400 MiB で、
/// コンテナの `mem_limit: 1024m` に対して 2 並列までなら収まる。公開 `/media-proxy` から並列リクエストを
/// 大量に投げられてもメモリを bound する (超過分は 503 busy)。
const MAX_CONCURRENT_MEDIA_JOBS: usize = 2;

/// `WebFinger` JRD 取得の本文上限 (64 KiB)。画像用 `max_bytes` (既定 25 MiB)
/// とは別枠 ── [`ProxyState::max_webfinger_bytes`] 参照。
pub(crate) const MAX_WEBFINGER_BYTES: usize = 64 * 1024;

/// media-proxy の共有状態。`Arc` で包んで axum の `State` に載せる。
#[derive(Debug)]
pub struct ProxyState {
    config: Config,
    http: Client,
    /// テスト / Docker 内連合テストで URL・DNS の SSRF ガードを緩める。
    /// 本番では `false`。
    allow_private_egress: bool,
    /// fetch / sanitize の同時実行数ゲート (OOM 防止)。
    jobs: Arc<Semaphore>,
}

impl ProxyState {
    /// 本番経路: `Config` から `reqwest::Client` を構築。
    ///
    /// DNS 解決後 IP の検証は既定で有効。テスト / Docker 内連合テストだけが
    /// `SAKURASATO_ALLOW_PRIVATE_EGRESS` で明示的に緩める (server と共通の
    /// 環境変数・判定関数を [`sakurasato_core::net_guard`] から使う)。
    pub fn from_config(config: Config) -> anyhow::Result<Arc<Self>> {
        let allow_private = sakurasato_core::net_guard::allow_private_egress_from_env();
        let http = http_client::build_client(allow_private)?;
        Ok(Arc::new(Self {
            config,
            http,
            allow_private_egress: allow_private,
            jobs: Arc::new(Semaphore::new(MAX_CONCURRENT_MEDIA_JOBS)),
        }))
    }

    /// fetch / sanitize ジョブの permit を **待たずに** 取る。取れなければ
    /// `None` (= 呼び出し側は 503 busy)。permit は処理完了まで保持する。
    pub fn try_acquire_job(&self) -> Option<SemaphorePermit<'_>> {
        self.jobs.try_acquire().ok()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// URL 文字列検査と DNS 解決後検査で共有するテスト専用 opt-in。
    pub fn allows_private_egress(&self) -> bool {
        self.allow_private_egress
    }

    /// `max_bytes`: ダウンロード本体 / 受信本文の上限。`usize` に丸めて返す。
    /// 環境変数で 64bit の巨大値が入っていた場合は `usize::MAX` にクランプ。
    pub fn max_bytes(&self) -> usize {
        usize::try_from(self.config.media_proxy.max_bytes).unwrap_or(usize::MAX)
    }

    /// `WebFinger` JRD 取得の本文上限。`max_bytes` (画像用、既定 25 MiB) とは
    /// 別枠の小さい固定値にする (F3)。
    ///
    /// `WebFinger` レスポンスは actor 解決のための小さな JSON で、通常数 KiB、
    /// リンクの多い実装でも数十 KiB に収まる。画像用上限を流用すると 25 MiB
    /// まで buffer してしまうため、64 KiB で頭打ちにする。
    pub fn max_webfinger_bytes(&self) -> usize {
        MAX_WEBFINGER_BYTES
    }

    /// `max_pixels`: デコード時の `image::Limits` に渡す。
    pub fn max_pixels(&self) -> u64 {
        self.config.media_proxy.max_pixels
    }

    /// 動画アップロードの最大バイト数。画像用 `max_bytes` とは別枠。
    pub fn max_video_bytes(&self) -> usize {
        usize::try_from(self.config.media_proxy.video.max_bytes).unwrap_or(usize::MAX)
    }

    /// 動画の最大再生時間 (ミリ秒)。コンテナヘッダの duration がこれを
    /// 超えたら [`crate::video_pipeline`] が reject する。
    pub fn max_video_duration_ms(&self) -> u64 {
        self.config
            .media_proxy
            .video
            .max_duration_secs
            .saturating_mul(1000)
    }
}
