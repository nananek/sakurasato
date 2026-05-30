//! 画像デコード → リサイズ → WebP 再エンコード。
//!
//! ここに `image` crate を閉じ込めることで、本パイプラインを呼び出す側
//! (= `fetch::handle` / `sanitize::handle`) は「バイト列 in → 安全化済み
//! バイト列 out」だけ見ればよい。
//!
//! # バリアント
//!
//! - `avatar` — 256x256 上限
//! - `thumbnail` — 320x320 上限
//! - `preview` — 1280x1280 上限
//! - `header` — 1500x500 上限 (Mastodon バナーサイズ)
//! - `emoji` — 128x128 上限 (Misskey 互換、`tag: Emoji.icon` で連合される)
//!
//! 上限以下の画像はそのまま、上限を超えるものは **アスペクト比を保ったまま**
//! 縮小する。出力は常に WebP (lossy, quality 80)。WebP に統一する利点:
//! - EXIF / XMP / ICC 等のメタデータが入らない (位置情報や端末識別の漏洩防止)
//! - 主要 Fediverse クライアントが対応 (Mastodon / Misskey / Pleroma)
//! - PNG より軽く JPEG より画質が安定 (写真 / 線画どちらでも妥当)
//!
//! # 防御
//!
//! - `image::Limits` で `max_image_width` / `max_image_height` / `max_alloc`
//!   を強制 ── decompression bomb が `decode()` 前に弾かれる。
//! - `max_pixels` は config から流し込み (`AppState::max_pixels`)。
//! - decode が失敗したら `unsupported_media` を返す ── 攻撃性かどうかは
//!   呼び出し側のメトリクスで切れる。

use std::io::Cursor;

use bytes::Bytes;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// 出力 WebP の品質目安 (image crate の WebP encoder は lossless 既定だが、
/// 0.25 系では `WebPEncoder::new_lossless` 形式のみ提供される ── lossy
/// quality を後で差し込むときのために 80 という値だけ載せておく)。
#[allow(dead_code)]
const WEBP_QUALITY: u8 = 80;

/// Hard ceiling: 各バリアントの画素数上限を別途設けて、設定ミスで巨大な
/// 出力を作らないようにする。
const HARD_MAX_DIMENSION: u32 = 4096;

/// バリアント (= 出力サイズの上限ボックス)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Avatar,
    Thumbnail,
    Preview,
    Header,
    /// カスタム絵文字 (M8)。Misskey の `emojis/<shortcode>.png` は実物が
    /// 128px 以下のものが多く、本実装も 128x128 ボックスに揃える。
    /// アニメーション GIF/APNG/animated WebP は現状の `image` crate での
    /// 再エンコード経路がフレームを 1 枚に落とすため、初回フレームのみ
    /// 残る (M8 PR1 のスコープ。CLAUDE.md M9 で再評価)。
    Emoji,
}

impl Variant {
    /// バリアントごとの (`max_width`, `max_height`)。
    pub fn max_box(self) -> (u32, u32) {
        match self {
            Self::Avatar => (256, 256),
            Self::Thumbnail => (320, 320),
            Self::Preview => (1280, 1280),
            Self::Header => (1500, 500),
            Self::Emoji => (128, 128),
        }
    }
}

/// 出力結果。`bytes` は WebP、`content_type` は固定 `image/webp`。
#[derive(Debug)]
pub struct ProcessedImage {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// バイト列を受け取り、デコード → リサイズ → WebP 再エンコードを行う。
///
/// `max_pixels` は config 由来の総画素数 (= width * height) 上限。
/// `image::Limits` の `max_alloc` に渡し、巨大解像度宣言の decompression bomb
/// を decode 前に弾く。
pub fn process(
    input: &[u8],
    variant: Variant,
    max_pixels: u64,
) -> Result<ProcessedImage, ApiError> {
    if input.is_empty() {
        return Err(ApiError::bad_request("empty_body", "input bytes are empty"));
    }

    // フォーマット推定 (magic bytes)。失敗時は `unsupported_media`。
    let reader = ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .map_err(|e| ApiError::unsupported_media("guess_format", format!("guess format: {e}")))?;

    // 既知の安全 format だけ受理。AVIF / HEIC 等は引いていない (deps を
    // 増やさない方針)。
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif) => {}
        Some(other) => {
            return Err(ApiError::unsupported_media(
                "unsupported_format",
                format!("format {other:?} is not supported"),
            ));
        }
        None => {
            return Err(ApiError::unsupported_media(
                "unknown_format",
                "could not detect image format from magic bytes",
            ));
        }
    }

    let mut reader = reader;
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(HARD_MAX_DIMENSION);
    limits.max_image_height = Some(HARD_MAX_DIMENSION);
    // max_alloc は image::Limits が許可する **メモリ確保の総量** 上限 (u64)。
    // max_pixels から RGBA 換算で 4 bytes/pixel として上限を決める。
    let max_bytes_alloc = max_pixels.saturating_mul(4);
    limits.max_alloc = Some(max_bytes_alloc);
    reader.limits(limits);

    let img = reader
        .decode()
        .map_err(|e| ApiError::unsupported_media("decode_failed", format!("decode: {e}")))?;

    let (max_w, max_h) = variant.max_box();
    let resized = if img.width() <= max_w && img.height() <= max_h {
        img
    } else {
        // `resize` はアスペクト比を保ち、box に収まるようにフィットさせる
        // (= サイズが「以下」になる)。`Lanczos3` で品質を稼ぐ。
        img.resize(max_w, max_h, FilterType::Lanczos3)
    };

    let (out_w, out_h) = (resized.width(), resized.height());
    // WebP encode。RGBA8 に揃えてから encoder に流す ── 一部 frame 形式は
    // encoder が拒否するため。
    let rgba = resized.into_rgba8();
    let mut out = Vec::with_capacity(64 * 1024);
    {
        let encoder = WebPEncoder::new_lossless(&mut out);
        encoder
            .encode(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| ApiError::internal("encode_failed", format!("webp encode: {e}")))?;
    }

    Ok(ProcessedImage {
        bytes: Bytes::from(out),
        content_type: "image/webp",
        width: out_w,
        height: out_h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(width, height, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            Rgba([(x % 256) as u8, (y % 256) as u8, 0, 255])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn rejects_empty_input() {
        let err = process(&[], Variant::Avatar, 1_000_000).unwrap_err();
        assert_eq!(err.reason, "empty_body");
    }

    #[test]
    fn rejects_non_image_bytes() {
        let err = process(b"not an image", Variant::Avatar, 1_000_000).unwrap_err();
        // `with_guessed_format` は magic bytes が無いと None を返し、
        // ImageReader::decode に到達するルートでは Err になる。
        assert!(matches!(err.reason, "decode_failed" | "unknown_format"));
    }

    #[test]
    fn avatar_within_bounds_kept_as_is() {
        let png = png_bytes(100, 80);
        let out = process(&png, Variant::Avatar, 10_000_000).unwrap();
        assert_eq!(out.width, 100);
        assert_eq!(out.height, 80);
        assert_eq!(out.content_type, "image/webp");
        assert!(!out.bytes.is_empty());
    }

    #[test]
    fn avatar_oversized_is_resized_to_box() {
        let png = png_bytes(2000, 1000);
        let out = process(&png, Variant::Avatar, 10_000_000).unwrap();
        // box は 256x256。アスペクト比保ったまま 256x128 に収まる。
        assert!(out.width <= 256);
        assert!(out.height <= 256);
        // 縦横比 (2:1) が保たれる。
        assert!(out.width >= out.height);
    }

    #[test]
    fn header_box_is_wide() {
        let png = png_bytes(3000, 3000);
        let out = process(&png, Variant::Header, 16_000_000).unwrap();
        assert!(out.width <= 1500);
        assert!(out.height <= 500);
    }

    #[test]
    fn emoji_box_is_128() {
        // Misskey サーバ由来の絵文字は 128px 前後が多い ─ 大きい入力は
        // アスペクト比保ったまま 128 ボックスに収める。
        let png = png_bytes(512, 256);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert!(out.width <= 128);
        assert!(out.height <= 128);
        // 2:1 のアスペクト比保持 (高さは幅の半分)。
        assert_eq!(out.height * 2, out.width);
    }

    #[test]
    fn emoji_small_input_passes_through() {
        let png = png_bytes(64, 64);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(out.width, 64);
        assert_eq!(out.height, 64);
        assert_eq!(out.content_type, "image/webp");
    }

    #[test]
    fn rejects_dimensions_above_hard_max() {
        // HARD_MAX_DIMENSION (4096) より大きい入力は decode 段で limits により拒否。
        let png = png_bytes(5000, 100);
        let err = process(&png, Variant::Preview, 100_000_000).unwrap_err();
        assert_eq!(err.reason, "decode_failed");
    }

    #[test]
    fn variant_serde_lowercase() {
        let s = serde_json::to_string(&Variant::Avatar).unwrap();
        assert_eq!(s, "\"avatar\"");
        let v: Variant = serde_json::from_str("\"preview\"").unwrap();
        assert_eq!(v, Variant::Preview);
    }
}
