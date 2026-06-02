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
//! - `emoji` — 512x512 上限 (`tag: Emoji.icon` で連合される)
//!
//! 上限以下の画像はそのまま、上限を超えるものは **アスペクト比を保ったまま**
//! 縮小する。出力は常に WebP (lossy, quality 80)。WebP に統一する利点:
//! - EXIF / XMP / ICC 等のメタデータが入らない (位置情報や端末識別の漏洩防止)
//! - 主要 Fediverse クライアントが対応 (Mastodon / Misskey / Pleroma)
//! - PNG より軽く JPEG より画質が安定 (写真 / 線画どちらでも妥当)
//!
//! # animated 対応 (#129)
//!
//! animated GIF / APNG / animated WebP は [`image`] crate の WebP encoder が
//! still 限定のため、従来は 1 フレーム目に潰れていた。本実装では:
//! 1. magic bytes / chunk 走査で animated か事前判別 (decode 不要)。
//! 2. animated と判定されたら [`image::AnimationDecoder`] で全 frame を decode、
//!    各 frame を variant ボックスに resize し、[`webp::AnimEncoder`] で
//!    animated WebP に再エンコード。
//! 3. 静止画 (= PNG / JPEG / 1 frame GIF / 1 frame WebP) は従来通り still WebP。
//!
//! 副作用として全 variant (avatar / header / thumbnail / preview / emoji) で
//! animated 入力がそのまま animated WebP として出る ── Mastodon / Misskey も
//! 同様にアバター / ヘッダで animated を許容しているため整合する。
//!
//! # 防御
//!
//! - `image::Limits` で `max_image_width` / `max_image_height` / `max_alloc`
//!   を強制 ── decompression bomb が `decode()` 前に弾かれる。
//! - `max_pixels` は config から流し込み (`AppState::max_pixels`)。
//! - animated は frame 数を [`MAX_ANIMATED_FRAMES`] で打ち切り、frame ごとに
//!   resize 直後の RGBA だけ保持 (= 入力 frame は drop 済) してメモリを抑える。
//! - decode が失敗したら `unsupported_media` を返す ── 攻撃性かどうかは
//!   呼び出し側のメトリクスで切れる。

use std::io::Cursor;

use bytes::Bytes;
use image::AnimationDecoder;
use image::ImageBuffer;
use image::ImageDecoder;
use image::ImageFormat;
use image::ImageReader;
use image::Rgba;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::{WebPDecoder, WebPEncoder};
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

/// animated 入力で受け付ける最大フレーム数。これを超えるアニメは reject。
/// 25 fps × 12 秒 ≒ 300 frame を目安。媒介できる範囲を超えるアニメは
/// そもそも emoji / avatar の用途ではないため、ここで CPU / メモリを守る。
const MAX_ANIMATED_FRAMES: usize = 300;

/// バリアント (= 出力サイズの上限ボックス)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Avatar,
    Thumbnail,
    Preview,
    Header,
    /// カスタム絵文字 (M8 / Issue #134)。Misskey / Mastodon ともに実勢で
    /// 256〜512px の素材が一般的で、128 まで落とすと `HiDPI` / Kitty graphics
    /// preview 枠で粗が目立つ。安全化責務 (decode → EXIF 剥がし → WebP 再
    /// エンコード) は維持しつつ box を 512 に上げ、ユーザがキュレートした
    /// 素材の元解像度をなるべく保つ ── 小さい入力 (= 32〜128px) はそのまま
    /// 通る (= 「以下なら resize しない」設計のため)。
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
            Self::Emoji => (512, 512),
        }
    }
}

/// 出力結果。`bytes` は WebP (animated/still いずれも `image/webp`)、
/// `width` / `height` は出力ボックスの寸法 (animated の場合 canvas 寸法)。
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
    let format = match reader.format() {
        Some(f @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif)) => {
            f
        }
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
    };

    // animated と判別される入力は専用パイプライン。JPEG / 静止 PNG / 静止
    // WebP / 静止 GIF は従来通りの still 経路。
    if is_animated_bytes(format, input) {
        process_animated(input, format, variant, max_pixels)
    } else {
        process_still(reader, variant, max_pixels)
    }
}

/// 静止画 (= 単一フレーム) 経路。従来実装の主流路。
fn process_still(
    reader: ImageReader<Cursor<&[u8]>>,
    variant: Variant,
    max_pixels: u64,
) -> Result<ProcessedImage, ApiError> {
    let mut reader = reader;
    reader.limits(make_limits(max_pixels));

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
    let bytes = encode_still_webp(&rgba)?;

    Ok(ProcessedImage {
        bytes,
        content_type: "image/webp",
        width: out_w,
        height: out_h,
    })
}

/// animated 経路 (#129)。decoder iterator から 1 frame ずつ取り出して resize
/// → `ResizedFrame` に蓄積する **単一パス** 実装。元バッファを Vec に溜め込んで
/// から resize するとピーク時に「`DecodedFrame` `Vec` + `ResizedFrame` `Vec`」の二重
/// 保持になるため、frame ごとに decode → resize → push → 元 buffer drop の
/// 順で進める。また `MAX_ANIMATED_FRAMES` 超過は `next()` を **呼ぶ前** に
/// 判定して、N+1 番目の decode が走らないようにする (=「意図 = `N` 件まで
/// 保管 + `N+1` 件目以降は decode しない」を実装上も担保)。
fn process_animated(
    input: &[u8],
    format: ImageFormat,
    variant: Variant,
    max_pixels: u64,
) -> Result<ProcessedImage, ApiError> {
    let limits = make_limits(max_pixels);
    let (resized_frames, canvas_w, canvas_h) =
        decode_and_resize_animation(input, format, limits, variant)?;

    if resized_frames.is_empty() {
        // animated と検出したが実は 0 frame だった ── 壊れた input。
        return Err(ApiError::unsupported_media(
            "decode_failed",
            "animated decoder yielded zero frames",
        ));
    }

    // 1 frame しか取れなかった場合は still と同等。animated WebP の
    // VP8X オーバーヘッドが無駄なので still encoder で出す。
    if resized_frames.len() == 1 {
        let only = &resized_frames[0];
        let bytes = encode_still_webp(&only.rgba)?;
        return Ok(ProcessedImage {
            bytes,
            content_type: "image/webp",
            width: canvas_w,
            height: canvas_h,
        });
    }

    let bytes = encode_animated_webp(canvas_w, canvas_h, &resized_frames)?;
    Ok(ProcessedImage {
        bytes,
        content_type: "image/webp",
        width: canvas_w,
        height: canvas_h,
    })
}

/// 静止画 1 枚 → WebP バイト列。`image` crate の lossless encoder。
fn encode_still_webp(rgba: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Result<Bytes, ApiError> {
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
    Ok(Bytes::from(out))
}

/// resize 後の frame。canvas 寸法に揃った RGBA + 表示時間 (ms)。
struct ResizedFrame {
    rgba: ImageBuffer<Rgba<u8>, Vec<u8>>,
    delay_ms: u32,
}

/// format に応じて decoder を開き、frame を単一パスで resize して
/// `Vec<ResizedFrame>` + canvas 寸法 (= 最初の frame の resized サイズ) を返す。
/// 各 decoder は `image::Frames<'_>` を yield するが、lifetime が decoder に
/// 紐付くため、format ごとに decoder のライフタイムを `process_frames` の
/// 呼び出し内に閉じる構造にしてある。
fn decode_and_resize_animation(
    input: &[u8],
    format: ImageFormat,
    limits: image::Limits,
    variant: Variant,
) -> Result<(Vec<ResizedFrame>, u32, u32), ApiError> {
    match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(Cursor::new(input)).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("gif open: {e}"))
            })?;
            decoder.set_limits(limits).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("gif limits: {e}"))
            })?;
            process_frames(decoder.into_frames(), variant)
        }
        ImageFormat::Png => {
            let mut decoder = PngDecoder::new(Cursor::new(input)).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("png open: {e}"))
            })?;
            decoder.set_limits(limits).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("png limits: {e}"))
            })?;
            let apng = decoder
                .apng()
                .map_err(|e| ApiError::unsupported_media("decode_failed", format!("apng: {e}")))?;
            process_frames(apng.into_frames(), variant)
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(Cursor::new(input)).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("webp open: {e}"))
            })?;
            decoder.set_limits(limits).map_err(|e| {
                ApiError::unsupported_media("decode_failed", format!("webp limits: {e}"))
            })?;
            process_frames(decoder.into_frames(), variant)
        }
        // is_animated_bytes が true を返したのに format がここに来るのは
        // ありえない。is_animated_bytes との不整合は internal error。
        other => Err(ApiError::internal(
            "decode_failed",
            format!("animated decoder requested for non-animated format {other:?}"),
        )),
    }
}

/// `frames` から 1 frame ずつ取り出し、その場で resize → `ResizedFrame` に
/// 蓄積する。canvas 寸法は 1 frame 目の aspect-preserving resize 結果で確定し、
/// 2 frame 目以降は `resize_exact` で同寸法に揃える (animated WebP の
/// VP8X canvas 前提)。
///
/// `MAX_ANIMATED_FRAMES` 超過は **`next()` を呼ぶ前** に判定する ── for ループ
/// だと `iter.next()` の後に body が走るため N+1 番目の decode が発生してしまう。
/// 明示 `loop { check; next; ... }` で N+1 件目以降は decode しない契約。
fn process_frames(
    mut frames: image::Frames<'_>,
    variant: Variant,
) -> Result<(Vec<ResizedFrame>, u32, u32), ApiError> {
    let mut out: Vec<ResizedFrame> = Vec::new();
    let mut canvas_w: u32 = 0;
    let mut canvas_h: u32 = 0;
    loop {
        if out.len() >= MAX_ANIMATED_FRAMES {
            return Err(ApiError::too_large(format!(
                "animated input exceeds {MAX_ANIMATED_FRAMES} frames",
            )));
        }
        let Some(frame_res) = frames.next() else {
            break;
        };
        let frame = frame_res.map_err(|e| {
            ApiError::unsupported_media("decode_failed", format!("frame decode: {e}"))
        })?;
        let delay_ms = compute_delay_ms(frame.delay());
        let buffer = frame.into_buffer();
        let resized = if out.is_empty() {
            // 1 frame 目: variant ボックスに aspect-preserving resize し canvas 確定。
            let r = resize_frame_buffer(&buffer, variant);
            canvas_w = r.width();
            canvas_h = r.height();
            r
        } else {
            // 2 frame 目以降: canvas 寸法に exact resize。
            resize_frame_exact(&buffer, canvas_w, canvas_h)
        };
        // `buffer` (= 元寸法の入力 frame) はこのスコープ末尾で drop。
        // = `out` には resize 後の小さい RGBA だけが残る = ピーク 2 重保持なし。
        out.push(ResizedFrame {
            rgba: resized,
            delay_ms,
        });
    }
    Ok((out, canvas_w, canvas_h))
}

/// `image::Delay` から表示時間 (ms) を取り出す。0 ms フレームは libwebp 側で
/// timestamp 衝突を起こすため最低 10 ms に底上げする (Misskey 等の観測値より
/// 下回らない安全マージン)。0 除算 (`den == 0`) も同じく 10 ms に倒す。
fn compute_delay_ms(delay: image::Delay) -> u32 {
    let (num, den) = delay.numer_denom_ms();
    let raw_ms = num.checked_div(den).unwrap_or(0);
    raw_ms.max(10)
}

/// アスペクト比を保ったままボックス内に収める resize (still と同じ挙動)。
fn resize_frame_buffer(
    buf: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    variant: Variant,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let (max_w, max_h) = variant.max_box();
    if buf.width() <= max_w && buf.height() <= max_h {
        return buf.clone();
    }
    let dyn_img = image::DynamicImage::ImageRgba8(buf.clone());
    dyn_img
        .resize(max_w, max_h, FilterType::Lanczos3)
        .into_rgba8()
}

/// canvas 寸法に対して exact 寸法で resize する版 (frame 間で寸法を揃える)。
fn resize_frame_exact(
    buf: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    canvas_w: u32,
    canvas_h: u32,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    if buf.width() == canvas_w && buf.height() == canvas_h {
        return buf.clone();
    }
    let dyn_img = image::DynamicImage::ImageRgba8(buf.clone());
    dyn_img
        .resize_exact(canvas_w, canvas_h, FilterType::Lanczos3)
        .into_rgba8()
}

/// `webp::AnimEncoder` で animated WebP を組み立てる。frame ごとの
/// timestamp は累積遅延 (ms) を渡す ── libwebp の API は frame の **終端**
/// timestamp を受け付ける形式。
fn encode_animated_webp(
    width: u32,
    height: u32,
    frames: &[ResizedFrame],
) -> Result<Bytes, ApiError> {
    let mut config = webp::WebPConfig::new()
        .map_err(|()| ApiError::internal("encode_failed", "WebPConfig::new failed"))?;
    config.lossless = 1;
    // 注: libwebp の lossless モードでは `quality` は視覚品質ではなく
    // **圧縮努力量** (0=低圧縮高速, 100=高圧縮低速) を意味する。emoji /
    // avatar 用途は通常 small payload なので「並み程度の努力量」= 80 で十分。
    config.quality = 80.0;

    let mut encoder = webp::AnimEncoder::new(width, height, &config);
    encoder.set_loop_count(0); // 0 = infinite loop (= GIF / APNG 既定相当)

    let mut timestamp_ms: i32 = 0;
    for frame in frames {
        let next_ts =
            timestamp_ms.saturating_add(i32::try_from(frame.delay_ms).unwrap_or(i32::MAX));
        let af = webp::AnimFrame::from_rgba(frame.rgba.as_raw(), width, height, timestamp_ms);
        encoder.add_frame(af);
        timestamp_ms = next_ts;
    }

    let memory = encoder
        .try_encode()
        .map_err(|e| ApiError::internal("encode_failed", format!("anim webp encode: {e:?}")))?;
    Ok(Bytes::copy_from_slice(&memory))
}

fn make_limits(max_pixels: u64) -> image::Limits {
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(HARD_MAX_DIMENSION);
    limits.max_image_height = Some(HARD_MAX_DIMENSION);
    // max_alloc は image::Limits が許可する **メモリ確保の総量** 上限 (u64)。
    // max_pixels から RGBA 換算で 4 bytes/pixel として上限を決める。
    let max_bytes_alloc = max_pixels.saturating_mul(4);
    limits.max_alloc = Some(max_bytes_alloc);
    limits
}

/// magic bytes / chunk 走査で animated かを判別する (decode 不要)。
///
/// - GIF: `GIF89a` で複数の Image Separator (0x2C) を持つ場合 animated。
///   `GIF87a` は仕様上 animated 不可だが念のため両 header を走査対象に。
/// - PNG: `acTL` chunk があれば APNG (spec: IDAT より前に必ず置かれる)。
/// - WebP: VP8X chunk の Animation bit (0x02) もしくは ANIM chunk があれば
///   animated。
/// - JPEG: animated 不可。
fn is_animated_bytes(format: ImageFormat, input: &[u8]) -> bool {
    match format {
        ImageFormat::Gif => has_multiple_gif_image_separators(input),
        ImageFormat::Png => has_apng_actl_chunk(input),
        ImageFormat::WebP => is_animated_webp(input),
        _ => false,
    }
}

/// GIF は Image Separator (0x2C) を 2 個以上含むかで判別。color table や
/// extension block の中に偶然 0x2C が出る確率は低くないが、false positive
/// しても animated 経路に流れ込んで `frames.len() == 1` で still fallback に
/// 倒れるだけなので「念のため広めに animated 判定する」方針で良い。
fn has_multiple_gif_image_separators(input: &[u8]) -> bool {
    let mut count = 0usize;
    for &b in input {
        if b == 0x2C {
            count += 1;
            if count >= 2 {
                return true;
            }
        }
    }
    false
}

/// APNG は `acTL` chunk が IDAT より前にある。chunk type は ASCII 4 バイト。
/// 走査は先頭 64KB に絞る (`acTL` は header 直後にあるため)。
fn has_apng_actl_chunk(input: &[u8]) -> bool {
    const ACTL: &[u8; 4] = b"acTL";
    let scan_to = input.len().min(64 * 1024);
    input[..scan_to].windows(4).any(|w| w == ACTL.as_slice())
}

/// WebP は RIFF container。`RIFF....WEBP` 直後の chunk を順に走査して
/// `VP8X` (Extended) を見つけ、その flags の Animation bit を読む。`ANIM`
/// chunk を直接見つけた場合も animated 確定。
fn is_animated_webp(input: &[u8]) -> bool {
    if input.len() < 12 {
        return false;
    }
    if &input[0..4] != b"RIFF" || &input[8..12] != b"WEBP" {
        return false;
    }
    let mut cursor = 12;
    while cursor + 8 <= input.len() {
        let chunk_type = &input[cursor..cursor + 4];
        // chunk size は little-endian u32。size + 1 の偶数 padding ルール
        // (spec) に従う。
        let size = u32::from_le_bytes([
            input[cursor + 4],
            input[cursor + 5],
            input[cursor + 6],
            input[cursor + 7],
        ]) as usize;
        if chunk_type == b"ANIM" {
            return true;
        }
        if chunk_type == b"VP8X" {
            // VP8X payload の 1 バイト目が flags。bit 1 (0x02) が Animation。
            if cursor + 8 < input.len() {
                let flags = input[cursor + 8];
                if flags & 0x02 != 0 {
                    return true;
                }
            }
        }
        // 次の chunk へ。size は奇数なら padding 1 バイト挿入される。
        let advance = 8usize.saturating_add(size).saturating_add(size & 1);
        cursor = cursor.saturating_add(advance);
    }
    false
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

    /// 単純な `GIF89a` (animated, 2 frame) を組み立てるヘルパ。GIF spec 中で
    /// 最小限のものを手で並べる。
    fn animated_gif_2_frames() -> Vec<u8> {
        // GIF89a header + Logical Screen Descriptor (no GCT) + 2 image
        // descriptors の手書きアセンブリは煩雑なので、`image` crate の
        // GifEncoder を借りる。
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame};
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for (r, g) in [(255, 0), (0, 255)] {
                let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
                    ImageBuffer::from_fn(16, 16, |_, _| Rgba([r, g, 0, 255]));
                let frame = Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(40, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        out
    }

    /// APNG (animated PNG, 2 frame) を作る。`image` crate には APNG encoder
    /// が無いので png crate 直叩きで組む ── ここでは APNG chunk を持つ
    /// バイト列を最小構成で書き下す。
    /// `image` crate 経由でも 0.25 で APNG decode はできるが encode できない。
    /// 代替: `png::Encoder` (image crate の依存) で APNG を組み立てる。
    fn animated_apng_2_frames() -> Vec<u8> {
        use png::Encoder;
        let mut out = Vec::new();
        {
            let mut encoder = Encoder::new(&mut out, 16, 16);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            // 2 frame の APNG として宣言。`set_animated(num_frames, num_plays)`。
            encoder.set_animated(2, 0).unwrap();
            encoder.set_frame_delay(40, 1000).unwrap();
            let mut writer = encoder.write_header().unwrap();
            let frame_a: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
            writer.write_image_data(&frame_a).unwrap();
            let frame_b: Vec<u8> = (0..16 * 16).flat_map(|_| [0u8, 255, 0, 255]).collect();
            writer.write_image_data(&frame_b).unwrap();
            writer.finish().unwrap();
        }
        out
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
    fn emoji_box_is_512() {
        // Issue #134: emoji box は 512x512。box を超える入力 (1024x512) は
        // アスペクト比を保ったまま 512x256 に収まる。
        let png = png_bytes(1024, 512);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert!(out.width <= 512);
        assert!(out.height <= 512);
        // 2:1 のアスペクト比保持 (高さは幅の半分)。
        assert_eq!(out.height * 2, out.width);
    }

    #[test]
    fn emoji_small_input_passes_through() {
        // 64x64 (box 以下) はそのまま通る ── 小サイズ素材を勝手にアップ
        // スケールしない。
        let png = png_bytes(64, 64);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(out.width, 64);
        assert_eq!(out.height, 64);
        assert_eq!(out.content_type, "image/webp");
    }

    #[test]
    fn emoji_mid_input_passes_through() {
        // Issue #134: 旧 128 box では潰されていた中サイズ (256x256 等) を
        // そのまま保持できることを確認。HiDPI / 高解像度 preview 用途の
        // 主目的の retain ケース。
        let png = png_bytes(256, 256);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(out.width, 256);
        assert_eq!(out.height, 256);
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

    // ─────── animated 入力 (#129) ───────

    #[test]
    fn animated_gif_emits_animated_webp() {
        let gif = animated_gif_2_frames();
        assert!(is_animated_bytes(ImageFormat::Gif, &gif));
        let out = process(&gif, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(out.content_type, "image/webp");
        // 出力 WebP が animated (= ANIM chunk または VP8X.animation flag) を
        // 持つこと。
        assert!(
            is_animated_webp(&out.bytes),
            "expected animated WebP output, got static"
        );
        // canvas 寸法は 128 box に収まる (16x16 入力なので 16x16 のまま)。
        assert_eq!(out.width, 16);
        assert_eq!(out.height, 16);
    }

    #[test]
    fn animated_apng_emits_animated_webp() {
        let apng = animated_apng_2_frames();
        assert!(
            has_apng_actl_chunk(&apng),
            "test fixture must contain acTL chunk"
        );
        let out = process(&apng, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(out.content_type, "image/webp");
        assert!(
            is_animated_webp(&out.bytes),
            "expected animated WebP output, got static"
        );
    }

    #[test]
    fn static_png_emits_still_webp() {
        // 静止 PNG は従来どおり still 経路 (= ANIM/VP8X.animation 無し)。
        let png = png_bytes(64, 64);
        let out = process(&png, Variant::Emoji, 1_000_000).unwrap();
        assert!(
            !is_animated_webp(&out.bytes),
            "static PNG must not become animated WebP"
        );
    }

    #[test]
    fn detect_apng_actl_chunk() {
        let still = png_bytes(16, 16);
        assert!(!has_apng_actl_chunk(&still));
        let apng = animated_apng_2_frames();
        assert!(has_apng_actl_chunk(&apng));
    }

    #[test]
    fn detect_animated_gif_separators() {
        let still = png_bytes(8, 8);
        // PNG bytes should not look like animated GIF
        assert!(!has_multiple_gif_image_separators(&still));
        let gif = animated_gif_2_frames();
        assert!(has_multiple_gif_image_separators(&gif));
    }

    /// 自前で組んだ animated WebP (= libwebp 経由) を入力に与えて、
    /// 再エンコード後も animated が維持されること。`is_animated_webp`
    /// 判別が VP8X flag / ANIM chunk のどちらかを拾えていることの roundtrip
    /// 検証も兼ねる。
    fn animated_webp_2_frames(w: u32, h: u32) -> Vec<u8> {
        let mut config = webp::WebPConfig::new().unwrap();
        config.lossless = 1;
        let mut encoder = webp::AnimEncoder::new(w, h, &config);
        encoder.set_loop_count(0);
        let red: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let blue: Vec<u8> = (0..w * h).flat_map(|_| [0u8, 0, 255, 255]).collect();
        encoder.add_frame(webp::AnimFrame::from_rgba(&red, w, h, 0));
        encoder.add_frame(webp::AnimFrame::from_rgba(&blue, w, h, 40));
        encoder.try_encode().unwrap().to_vec()
    }

    #[test]
    fn animated_webp_input_round_trips() {
        let bytes = animated_webp_2_frames(16, 16);
        assert!(
            is_animated_webp(&bytes),
            "test fixture must be detected as animated WebP"
        );
        let out = process(&bytes, Variant::Emoji, 1_000_000).unwrap();
        assert!(is_animated_webp(&out.bytes));
    }

    #[test]
    fn frame_count_exceeding_max_is_rejected() {
        // MAX_ANIMATED_FRAMES + 1 frame の GIF を組んで too_large が返ることを
        // 確認する。N+1 frame 目の decode が走らない契約は process_frames
        // のロジック側でカバー (= ここではエラー path に倒れるかどうかだけ)。
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame};
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..=MAX_ANIMATED_FRAMES {
                #[allow(clippy::cast_possible_truncation)]
                let shade = (i % 256) as u8;
                let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
                    ImageBuffer::from_fn(8, 8, |_, _| Rgba([shade, 0, 0, 255]));
                let frame = Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(40, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        let err = process(&out, Variant::Emoji, 10_000_000).unwrap_err();
        assert_eq!(err.reason, "too_large");
    }

    #[test]
    fn single_frame_apng_falls_back_to_still() {
        // APNG (= acTL chunk あり) だが frame は 1 枚だけ。is_animated_bytes が
        // animated と判定 → process_animated に入る → resized_frames.len() == 1
        // で still encoder にフォールバック。出力 WebP は VP8X.animation flag /
        // ANIM chunk を **持たない** ことを確認 (= 余計な animated container を
        // 巻かない)。
        use png::Encoder;
        let mut out = Vec::new();
        {
            let mut encoder = Encoder::new(&mut out, 16, 16);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(1, 0).unwrap();
            encoder.set_frame_delay(40, 1000).unwrap();
            let mut writer = encoder.write_header().unwrap();
            let frame: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
            writer.write_image_data(&frame).unwrap();
            writer.finish().unwrap();
        }
        // 前提: acTL chunk が乗っているので animated 判定が走る。
        assert!(has_apng_actl_chunk(&out));
        let result = process(&out, Variant::Emoji, 1_000_000).unwrap();
        assert_eq!(result.content_type, "image/webp");
        assert!(
            !is_animated_webp(&result.bytes),
            "1-frame APNG must fall back to still WebP"
        );
    }

    #[test]
    fn compute_delay_ms_floors_to_10ms() {
        use image::Delay;
        // 0 ms (例: 即座次フレームを宣言する 0 delay) → 10 ms に底上げ
        assert_eq!(compute_delay_ms(Delay::from_numer_denom_ms(0, 1)), 10);
        // 5 ms (10 ms 未満) → 10 ms に底上げ
        assert_eq!(compute_delay_ms(Delay::from_numer_denom_ms(5, 1)), 10);
        // 40 ms (常識的な GIF 25 fps) はそのまま
        assert_eq!(compute_delay_ms(Delay::from_numer_denom_ms(40, 1)), 40);
        // 100 ms (10 fps) もそのまま
        assert_eq!(compute_delay_ms(Delay::from_numer_denom_ms(100, 1)), 100);
    }

    #[test]
    fn animated_resize_preserves_canvas_box() {
        // 大きめ 1024x512 入力 → emoji 512 box にアスペクト比保持で収まる
        // (= 512x256)。frame 数は 2。box を超える入力を選ぶことで
        // resize 経路 (frame ごとに canvas 寸法へ揃える) を実テストする。
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame};
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for (r, g) in [(255, 0), (0, 255)] {
                let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
                    ImageBuffer::from_fn(1024, 512, |_, _| Rgba([r, g, 0, 255]));
                let frame = Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(40, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        // max_pixels は frame バッファ 1024×512×4 = 2_097_152 を許容できる
        // よう余裕を持って 8M。
        let result = process(&out, Variant::Emoji, 8_000_000).unwrap();
        assert!(result.width <= 512);
        assert!(result.height <= 512);
        assert_eq!(result.width, result.height * 2); // 2:1 aspect
        assert!(is_animated_webp(&result.bytes));
    }
}
