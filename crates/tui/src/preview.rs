//! ファイルピッカ用のローカル画像プレビューキャッシュ (M7)。
//!
//! `image_cache` がリモート URL (= server → media-proxy 経由) を扱うのに対し、
//! こちらは **ローカルファイル** をデコードして `Protocol` にする。アップロード
//! **前** に「これでいい?」と TUI でプレビューするための機構。
//!
//! 設計方針:
//! - `ImageCache` と同じく `Mutex<LruCache<PathBuf, ImageState>>` で多重 spawn 防止。
//! - 画素数 / ファイルサイズに `image::Limits` を当てる ── ユーザ自身が選んだ
//!   ファイルとはいえ、巨大画像を decode して TUI が落ちないように。
//! - decode は `tokio::task::spawn_blocking` で行う ── `image` crate の decode は
//!   CPU バウンドで、async タスク内で動かすと runtime が詰まる。
//!
//! プレビュー失敗 (非画像 / 巨大 / 破損) はステータスバーに出すだけで、
//! アップロードのブロッキング条件にはしない (= 視覚刺激抑制モードのユーザは
//! プレビュー無しで送れる)。

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::ImageReader;
use lru::LruCache;
use ratatui::layout::{Rect, Size};
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use tracing::{debug, warn};

/// 1 ファイルあたりの上限 (= 25 MiB)。`server::config::MediaProxyConfig::max_bytes`
/// と揃えるべきだが、起動時に config を持たないので決め打ち。
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// 失敗後の再試行 cool-down。
const RETRY_AFTER: Duration = Duration::from_secs(10);

const CACHE_CAP: usize = 16;

const MAX_DIMENSION: u32 = 4096;

/// プレビュー entry の状態。`image_cache::ImageState` と意図的に揃えてある。
#[derive(Clone)]
pub enum PreviewState {
    Loading,
    Ready(Arc<Protocol>),
    /// 動画ファイル (`.mp4` / `.webm`)。Kitty graphics protocol は静止画向け
    /// のため実再生・サムネイル生成はせず、ファイルサイズのみ表示する
    /// プレースホルダに倒す (ポスターフレーム抽出は別issue)。
    Video {
        size: u64,
    },
    Failed {
        until: Instant,
        reason: String,
    },
}

impl std::fmt::Debug for PreviewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loading => write!(f, "Loading"),
            Self::Ready(_) => write!(f, "Ready(<Protocol>)"),
            Self::Video { size } => f.debug_struct("Video").field("size", size).finish(),
            Self::Failed { until, reason } => f
                .debug_struct("Failed")
                .field("until", until)
                .field("reason", reason)
                .finish(),
        }
    }
}

/// 拡張子から動画ファイルかどうかを判定する。
fn is_video_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "webm")
    )
}

#[derive(Clone)]
pub struct PreviewCache {
    inner: Arc<Mutex<LruCache<PathBuf, PreviewState>>>,
    picker: Option<Arc<Picker>>,
}

impl std::fmt::Debug for PreviewCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewCache")
            .field("picker_initialized", &self.picker.is_some())
            .field("entries", &self.entry_count())
            .finish_non_exhaustive()
    }
}

impl PreviewCache {
    pub fn new(picker: Option<Picker>) -> Self {
        let cap = NonZeroUsize::new(CACHE_CAP).expect("non-zero");
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
            picker: picker.map(Arc::new),
        }
    }

    /// 端末の画像プロトコル対応有無のみを返す。
    ///
    /// 視覚刺激抑制 (M9 PR2) の要素別 on/off は呼び出し側 (= `ui::render_picker_preview`)
    /// が `app.suppression.preview` と AND を取って判断する ── キャッシュ層
    /// 自体は「端末が画像を出せるか」だけを返す責務に揃える。
    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub fn entry_count(&self) -> usize {
        self.inner.lock().map_or(0, |c| c.len())
    }

    pub fn get(&self, path: &Path) -> Option<Arc<Protocol>> {
        let mut cache = self.inner.lock().ok()?;
        match cache.get(path) {
            Some(PreviewState::Ready(p)) => Some(p.clone()),
            _ => None,
        }
    }

    pub fn state(&self, path: &Path) -> Option<PreviewState> {
        let mut cache = self.inner.lock().ok()?;
        cache.get(path).cloned()
    }

    /// `path` を解決し未取得 / 期限切れなら decode タスクを spawn する。
    /// 動画ファイルは decode を試みず、ファイルサイズだけ stat して
    /// [`PreviewState::Video`] に直行する (= 静止画デコードパスを通さない)。
    pub fn ensure(&self, path: &Path, size: Rect) {
        if is_video_path(path) {
            self.ensure_video(path);
            return;
        }
        let Some(picker) = self.picker.clone() else {
            return;
        };
        let needs_decode = {
            let Ok(mut cache) = self.inner.lock() else {
                return;
            };
            match cache.peek(path) {
                Some(PreviewState::Ready(_) | PreviewState::Loading) => false,
                Some(PreviewState::Failed { until, .. }) if Instant::now() < *until => false,
                _ => {
                    cache.put(path.to_path_buf(), PreviewState::Loading);
                    true
                }
            }
        };
        if !needs_decode {
            return;
        }
        let inner = self.inner.clone();
        let path_owned = path.to_path_buf();
        tokio::spawn(async move {
            let outcome = decode_local(&path_owned, &picker, size).await;
            let Ok(mut cache) = inner.lock() else { return };
            match outcome {
                Ok(proto) => {
                    cache.put(path_owned, PreviewState::Ready(Arc::new(proto)));
                }
                Err(err) => {
                    warn!(path = %path_owned.display(), error = %err, "preview decode failed");
                    cache.put(
                        path_owned,
                        PreviewState::Failed {
                            until: Instant::now() + RETRY_AFTER,
                            reason: err,
                        },
                    );
                }
            }
        });
    }

    /// 動画ファイル用の軽量経路。decode はせず stat のみ行う。
    fn ensure_video(&self, path: &Path) {
        let needs_stat = {
            let Ok(mut cache) = self.inner.lock() else {
                return;
            };
            match cache.peek(path) {
                Some(PreviewState::Video { .. } | PreviewState::Loading) => false,
                Some(PreviewState::Failed { until, .. }) if Instant::now() < *until => false,
                _ => {
                    cache.put(path.to_path_buf(), PreviewState::Loading);
                    true
                }
            }
        };
        if !needs_stat {
            return;
        }
        let inner = self.inner.clone();
        let path_owned = path.to_path_buf();
        tokio::spawn(async move {
            let outcome = tokio::fs::metadata(&path_owned).await;
            let Ok(mut cache) = inner.lock() else { return };
            match outcome {
                Ok(meta) => {
                    cache.put(path_owned, PreviewState::Video { size: meta.len() });
                }
                Err(err) => {
                    cache.put(
                        path_owned,
                        PreviewState::Failed {
                            until: Instant::now() + RETRY_AFTER,
                            reason: format!("stat: {err}"),
                        },
                    );
                }
            }
        });
    }
}

async fn decode_local(path: &Path, picker: &Picker, size: Rect) -> Result<Protocol, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("stat: {e}"))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file too large: {} bytes (max {MAX_FILE_BYTES})",
            metadata.len()
        ));
    }
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    let path_owned = path.to_path_buf();

    // image::decode is CPU-heavy; offload to a blocking pool.
    let bytes = tokio::fs::read(&path_owned)
        .await
        .map_err(|e| format!("read: {e}"))?;

    let dyn_img = tokio::task::spawn_blocking(move || -> Result<image::DynamicImage, String> {
        let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| format!("guess format: {e}"))?;
        let mut limits = image::Limits::no_limits();
        limits.max_alloc = Some(64 * 1024 * 1024);
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        reader.limits(limits);
        reader.decode().map_err(|e| format!("decode: {e}"))
    })
    .await
    .map_err(|e| format!("blocking task join: {e}"))??;

    let target = Size::new(size.width.max(1), size.height.max(1));
    let proto = picker
        .new_protocol(dyn_img, target, Resize::Fit(None))
        .map_err(|e| format!("protocol: {e}"))?;
    debug!(path = %path.display(), "preview ready");
    Ok(proto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cache_is_noop() {
        let cache = PreviewCache::new(None);
        assert!(!cache.enabled());
        cache.ensure(Path::new("/tmp/x.png"), Rect::new(0, 0, 10, 10));
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.get(Path::new("/tmp/x.png")).is_none());
    }
}
