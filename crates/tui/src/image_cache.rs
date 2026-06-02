//! 画像キャッシュとフェッチタスク。
//!
//! TUI はタイムラインに乗ったアバター URL を見るたびに本キャッシュへ
//! `ensure()` で問い合わせる。未取得なら fetch task を `tokio::spawn` し、
//! ローカル API (= server → media-proxy 経由) でバイト列を取り、
//! [`image::ImageReader`] でデコード後 [`ratatui_image::Picker`] で
//! プロトコル化して `LruCache` に積む。再描画時は同じ URL を `get()` で
//! 引いて Image widget へ。
//!
//! # M6 で変わったこと (Issue #36 解消)
//!
//! 以前 (M5 PR2) は TUI ホストプロセスから直接 HTTPS GET していた。
//! M6 では本キャッシュは **ローカル API しか叩かない**:
//!
//! 1. TUI → server `/api/v1/media/proxy` (Unix socket, Bearer 認証)
//! 2. server → media-proxy (Unix socket)
//! 3. media-proxy → 外部 GET
//! 4. media-proxy が WebP に再エンコードして返す
//!
//! 利点:
//! - **SSRF**: TUI ホストプロセスから外向き接続が消える ── LAN / クラウド
//!   IMDS / DNS rebinding が media-proxy の隔離コンテナだけに閉じる。
//! - **デコード爆弾耐性**: media-proxy が一度 decode し、サイズ・画素数を
//!   絞った WebP に再エンコードしてから返す。TUI 側の `image` crate は
//!   信頼済みバイト列だけ食う ── ただし多層防御として `image::Limits` は
//!   従来通り掛けたまま。
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
use sakurasato_core::net_guard::host_blocked;
use tracing::{debug, warn};

use crate::client::LocalApi;

/// `Failed` 後に同じ URL を再試行可能にするまでの cool-down。
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// LRU 容量。お一人様 TUI なので 64 ホスト分くらいで十分。
const CACHE_CAP: usize = 64;
/// 画像の最大幅/高さ (ピクセル)。media-proxy 側で `variant=avatar` は
/// 256x256 上限になっているが、多層防御として TUI 側でも decode 時に制限を
/// 掛ける ── server 経路を経ない攻撃 (= 別経路でキャッシュに突っ込まれる
/// 可能性) は無いが、image crate の Limits は常時オンが正しい運用。
const MAX_IMAGE_DIMENSION: u32 = 4096;
/// media-proxy に頼むバリアント。呼び出し側が用途に応じて選ぶ:
///
/// - `avatar` (256×256, 既定) ── アイコン / 絵文字 (= 小サイズ用途で十分)
/// - `thumbnail` (320×320) ── リスト中のサムネイル
/// - `preview` (1280×1280) ── Note 詳細モーダルの添付プレビュー (Issue #133)
/// - `header` (1500×500) ── プロフィール画像
///
/// media-proxy 側で同じ名前の variant に解決される ([`crate::client::LocalApi::fetch_proxy_image`])。
/// 未知の variant が渡ったときは server 側で 400 が返るため、呼び出し側で
/// 文字列をハードコードせず本モジュールの定数を使う。
pub const VARIANT_AVATAR: &str = "avatar";
pub const VARIANT_PREVIEW: &str = "preview";

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
    api: Option<LocalApi>,
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("picker_initialized", &self.picker.is_some())
            .field("api_attached", &self.api.is_some())
            .field("entries", &self.entry_count())
            .finish_non_exhaustive()
    }
}

impl ImageCache {
    /// 新しいキャッシュ。`picker` が `None` のときは画像表示無効化モード
    /// (= ensure / get が no-op)。`api` も渡されていないと取得経路が無いので
    /// 同じく無効化扱いになる。
    pub fn new(picker: Option<Picker>, api: Option<LocalApi>) -> Self {
        let cap = NonZeroUsize::new(CACHE_CAP).expect("non-zero cap");
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cap))),
            picker: picker.map(Arc::new),
            api,
        }
    }

    /// 画像表示が有効か (= `Picker` と `LocalApi` が両方利用可能か)。
    pub fn enabled(&self) -> bool {
        self.picker.is_some() && self.api.is_some()
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

    /// `url` が未取得 / 期限切れ failed なら fetch task を spawn する
    /// (= `variant = "avatar"` で 256×256 上限)。アバター / 絵文字経路で使う。
    pub fn ensure(&self, url: &str, size: Rect) {
        self.ensure_with_variant(url, size, VARIANT_AVATAR);
    }

    /// `ensure` のバリアント可変版。Issue #133 (4) 添付プレビューで
    /// `preview` (1280×1280) を渡すために生やした。`variant` は
    /// [`VARIANT_AVATAR`] / [`VARIANT_PREVIEW`] などの定数を渡す。
    ///
    /// キャッシュキーは `url` のみで、同じ URL を異なる variant で要求すると
    /// **最初の variant の結果が再利用される**。お一人様 TUI ではアバター・
    /// 絵文字・添付で URL が重複する場面は想定されない (= 添付は AP `Document`
    /// 由来、絵文字は `Emoji.icon.url` 由来、actor icon は `actor.icon_url`
    /// 由来で名前空間が衝突しない)。将来衝突を許す場合は cache key を
    /// `(url, variant)` に拡張する。
    pub fn ensure_with_variant(&self, url: &str, size: Rect, variant: &'static str) {
        let (Some(picker), Some(api)) = (self.picker.clone(), self.api.clone()) else {
            return;
        };
        if vet_url(url).is_none() {
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
        let url_owned = url.to_string();
        tokio::spawn(async move {
            let outcome = fetch_and_decode(&api, &url_owned, &picker, size, variant).await;
            let Ok(mut cache) = inner.lock() else {
                return;
            };
            match outcome {
                Ok(proto) => {
                    cache.put(url_owned, ImageState::Ready(Arc::new(proto)));
                }
                Err(err) => {
                    warn!(%url_owned, variant, error = %err, "media fetch failed");
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

/// URL が画像取得に許容できるか。スキーム制限 + SSRF allowlist の両方を通す。
/// 通過すれば `Some(parsed)` を返す。
///
/// **多層防御**: 実 SSRF 検査は media-proxy で再度行うが、ここでも弾く
/// ことで「無駄な UDS 往復」を節約する。CLAUDE.md §7 でも「同じガード
/// 関数を全経路から呼ぶ」原則。
fn vet_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return None;
    }
    if let Some(reason) = host_blocked(&parsed) {
        debug!(url = %parsed, reason, "avatar URL blocked by net_guard");
        return None;
    }
    Some(parsed)
}

async fn fetch_and_decode(
    api: &LocalApi,
    url: &str,
    picker: &Picker,
    size: Rect,
    variant: &str,
) -> Result<Protocol, String> {
    let target = Size::new(size.width, size.height);
    let parsed = vet_url(url).ok_or_else(|| "blocked URL".to_string())?;
    debug!(url = %parsed, variant, "fetch media via local API");

    let bytes = api
        .fetch_proxy_image(parsed.as_str(), variant)
        .await
        .map_err(|e| format!("media-proxy: {e}"))?;

    // media-proxy 経由のバイト列はすでに WebP 再エンコード済みだが、
    // 多層防御として decompression bomb 防御の `Limits` は掛けたままにする。
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("guess format: {e}"))?;
    let mut limits = image::Limits::no_limits();
    limits.max_alloc = Some(64 * 1024 * 1024); // 64 MiB
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
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
    fn vet_url_blocks_non_http_schemes() {
        assert!(vet_url("https://example.com/a.png").is_some());
        assert!(vet_url("http://example.com/a.png").is_some());
        assert!(vet_url("file:///etc/passwd").is_none());
        assert!(vet_url("data:image/png;base64,AAA").is_none());
        assert!(vet_url("ftp://example.com/a.png").is_none());
        assert!(vet_url("not a url").is_none());
    }

    #[test]
    fn vet_url_blocks_ssrf_targets() {
        // SSRF allowlist (sakurasato_core::net_guard 経由) が効くこと。
        assert!(vet_url("http://127.0.0.1/x").is_none());
        assert!(vet_url("http://10.0.0.1/x").is_none());
        assert!(vet_url("http://192.168.1.1/x").is_none());
        assert!(vet_url("http://169.254.169.254/latest/meta-data/").is_none());
        assert!(vet_url("http://localhost/x").is_none());
        assert!(vet_url("http://postgres.local/x").is_none());
        assert!(vet_url("http://[::1]/x").is_none());
        assert!(vet_url("http://[fe80::1]/x").is_none());
        // 公開 IP は通る。
        assert!(vet_url("https://1.1.1.1/x").is_some());
        assert!(vet_url("https://example.com/x").is_some());
    }

    #[test]
    fn disabled_cache_is_noop() {
        let cache = ImageCache::new(None, None);
        assert!(!cache.enabled());
        cache.ensure("https://example.com/a.png", Rect::new(0, 0, 3, 2));
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.get("https://example.com/a.png").is_none());
    }
}
