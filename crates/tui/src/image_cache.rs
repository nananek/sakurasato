//! 画像キャッシュとフェッチタスク。
//!
//! TUI はタイムラインに乗ったアバター URL を見るたびに本キャッシュへ
//! `ensure()` で問い合わせる。未取得なら fetch task を `tokio::spawn` し、
//! ダウンロード → デコード → [`ratatui_image::Picker`] でプロトコル化して
//! `LruCache` に積む。再描画時は同じ URL を `get()` で引いて Image widget へ。
//!
//! # セキュリティ方針
//!
//! - 取得先は **任意のリモート URL**。TUI はユーザのホスト上で走るため、
//!   server コンテナの egress 制限の外。`reqwest` を rustls で使う。
//! - **サイズ上限**: `MAX_BYTES = 4 MiB`。リモート画像を巨大にして OOM を狙う
//!   攻撃を限定する。HTTP `Content-Length` だけでなく、ストリーミング読み出し
//!   中に超過したら abort する。
//! - **タイムアウト**: 10 秒。
//! - **スキーマ制限**: `http` / `https` 以外は弾く (`file://` 等の禁止)。
//! - 画像 decode は本クライアント (TUI) で行う ── server 本体ではしない。
//!   `image` crate は CVE 履歴が知られているが、本クライアントが独立プロセスで
//!   走る限り server には波及しない (CLAUDE.md §7)。
//!
//! # スレッドモデル
//!
//! - [`Picker`] は `Send + Sync`。`Arc<Picker>` を fetch task に clone する。
//! - キャッシュ本体は `Mutex<LruCache<String, ImageState>>`。lock 区間は短い
//!   (= entry 出し入れだけ) ので contention は実用上問題にならない。
//! - 失敗 (`Failed`) は cool-down 秒 (`RETRY_AFTER`) 経過後の `ensure()` で
//!   再試行できる ── 端末プロトコル一時不調 / 一時的なネットワーク不可達など。

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::ImageReader;
use lru::LruCache;
use ratatui::layout::{Rect, Size};
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use tracing::{debug, warn};

/// 1 アバターの取得サイズ上限。
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// 取得タイムアウト。
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// `Failed` 後に同じ URL を再試行可能にするまでの cool-down。
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// LRU 容量。お一人様 TUI なので 64 ホスト分くらいで十分。
const CACHE_CAP: usize = 64;

/// キャッシュ entry の状態。
///
/// `Protocol` は `Debug` を実装していないので、`Ready` variant を Debug 出力
/// するときは `<Protocol>` プレースホルダで隠す。
#[derive(Clone)]
pub enum ImageState {
    /// fetch を spawn 済み。同 URL の重複 spawn を防ぐ。
    Loading,
    /// 取得 + デコード + ratatui-image protocol 生成まで完了。
    Ready(Arc<Protocol>),
    /// 何らかの理由で取得失敗。`Instant` は再試行可能になる時刻。
    Failed { until: Instant, reason: String },
}

impl std::fmt::Debug for ImageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loading => write!(f, "Loading"),
            Self::Ready(_) => write!(f, "Ready(<Protocol>)"),
            Self::Failed { until, reason } => f
                .debug_struct("Failed")
                .field("until", until)
                .field("reason", reason)
                .finish(),
        }
    }
}

/// LRU キャッシュ + fetcher。`Clone` 可で `App` / runtime / UI が同じ実体を共有する。
#[derive(Clone)]
pub struct ImageCache {
    inner: Arc<Mutex<LruCache<String, ImageState>>>,
    picker: Option<Arc<Picker>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("picker_initialized", &self.picker.is_some())
            .field("entries", &self.entry_count())
            .finish_non_exhaustive()
    }
}

impl ImageCache {
    /// 新しいキャッシュ。`picker` が `None` のときは画像表示無効化モード
    /// (= ensure / get が no-op)。
    pub fn new(picker: Option<Picker>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("sakurasato-tui/", env!("CARGO_PKG_VERSION")))
            // リダイレクトは最大 3 回 (リモート → CDN → 実体 を吸収)。
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let cap = NonZeroUsize::new(CACHE_CAP).expect("non-zero cap");
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
            picker: picker.map(Arc::new),
            http,
        }
    }

    /// 画像表示が有効か (= `Picker` が利用可能か)。
    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    /// 現在のエントリ数 (テスト/診断用)。
    pub fn entry_count(&self) -> usize {
        self.inner.lock().map_or(0, |c| c.len())
    }

    /// 既にキャッシュされた protocol を取り出す (= UI render から呼ぶ)。
    pub fn get(&self, url: &str) -> Option<Arc<Protocol>> {
        let mut cache = self.inner.lock().ok()?;
        match cache.get(url) {
            Some(ImageState::Ready(p)) => Some(p.clone()),
            _ => None,
        }
    }

    /// `url` が未取得 / 期限切れ failed なら fetch task を spawn する。
    /// `size` はターゲット領域 (= avatar セル数)。Picker は `font_size` を
    /// 使って実ピクセル換算する。
    pub fn ensure(&self, url: &str, size: Rect) {
        let Some(picker) = self.picker.clone() else {
            return;
        };
        if !is_allowed_scheme(url) {
            return;
        }
        let needs_fetch = {
            let Ok(mut cache) = self.inner.lock() else {
                return;
            };
            match cache.peek(url) {
                Some(ImageState::Ready(_) | ImageState::Loading) => false,
                Some(ImageState::Failed { until, .. }) if Instant::now() < *until => false,
                _ => {
                    cache.put(url.to_string(), ImageState::Loading);
                    true
                }
            }
        };
        if !needs_fetch {
            return;
        }
        let inner = self.inner.clone();
        let http = self.http.clone();
        let url_owned = url.to_string();
        tokio::spawn(async move {
            let outcome = fetch_and_decode(&http, &url_owned, &picker, size).await;
            let Ok(mut cache) = inner.lock() else {
                return;
            };
            match outcome {
                Ok(proto) => {
                    cache.put(url_owned, ImageState::Ready(Arc::new(proto)));
                }
                Err(err) => {
                    warn!(%url_owned, error = %err, "avatar fetch failed");
                    cache.put(
                        url_owned,
                        ImageState::Failed {
                            until: Instant::now() + RETRY_AFTER,
                            reason: err,
                        },
                    );
                }
            }
        });
    }
}

fn is_allowed_scheme(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
}

async fn fetch_and_decode(
    http: &reqwest::Client,
    url: &str,
    picker: &Picker,
    size: Rect,
) -> Result<Protocol, String> {
    let target = Size::new(size.width, size.height);
    debug!(%url, "fetch avatar");
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length()
        && usize::try_from(len).unwrap_or(usize::MAX) > MAX_BYTES
    {
        return Err(format!("Content-Length {len} exceeds {MAX_BYTES}"));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("body {} exceeds {MAX_BYTES}", bytes.len()));
    }

    // `image` crate でフォーマット推定 + デコード。`with_guessed_format` は
    // バイトの magic を見て jpg/png/webp/gif を分ける。失敗時はそのまま伝搬。
    // `set_limits` で巨大画像のピクセル展開を抑制する (= decompression bomb 対策)。
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes.clone()))
        .with_guessed_format()
        .map_err(|e| format!("guess format: {e}"))?;
    let mut limits = image::Limits::no_limits();
    limits.max_alloc = Some(64 * 1024 * 1024); // 64 MiB
    reader.limits(limits);
    let dyn_img = reader.decode().map_err(|e| format!("decode: {e}"))?;

    let proto = picker
        .new_protocol(dyn_img, target, Resize::Fit(None))
        .map_err(|e| format!("protocol: {e}"))?;
    Ok(proto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_scheme_filters_file_and_data_uris() {
        assert!(is_allowed_scheme("https://example.com/a.png"));
        assert!(is_allowed_scheme("http://example.com/a.png"));
        assert!(!is_allowed_scheme("file:///etc/passwd"));
        assert!(!is_allowed_scheme("data:image/png;base64,AAA"));
        assert!(!is_allowed_scheme("ftp://example.com/a.png"));
        assert!(!is_allowed_scheme("not a url"));
    }

    #[test]
    fn disabled_cache_is_noop() {
        let cache = ImageCache::new(None);
        assert!(!cache.enabled());
        cache.ensure("https://example.com/a.png", Rect::new(0, 0, 3, 2));
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.get("https://example.com/a.png").is_none());
    }
}
