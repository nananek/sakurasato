//! MP4 (ISOBMFF) / `WebM` (Matroska/EBML) のコンテナメタデータ無害化。
//!
//! 画像 ([`crate::image_pipeline`]) と異なり、動画は全再エンコードを行わない
//! (計算コストが見合わない)。代わりに **コンテナ内のメタデータ領域だけを
//! 同一バイト長のままインプレースで無害化** する:
//!
//! - MP4: `udta` / `meta` / `uuid` ボックスの fourcc を `free` に書き換え、
//!   payload をゼロ埋めする。`free` / `skip` は仕様上プレイヤーが無条件で
//!   スキップする「空き領域」ボックスなので安全。
//! - `WebM`: `Tags` / `Attachments` / `Chapters` エレメント、`Info` 内の
//!   `WritingApp` / `MuxingApp` エレメントの ID を `Void` (`0xEC`, EBML で
//!   「内容を無視してよいプレースホルダ」と規定済み) に書き換え、payload を
//!   ゼロ埋めする。
//!
//! # なぜ「削除して詰め直す」実装をしないか
//!
//! MP4 の `stco` / `co64` (サンプルチャンクオフセットテーブル) は **ファイル
//! 先頭からの絶対バイトオフセット** で `mdat` 内のサンプル位置を指す。
//! `udta` / `meta` を本当に削除すると `moov` 全体が縮み、後続の `mdat` の
//! 絶対位置がズレて `stco` / `co64` の値と実際のサンプル位置が食い違い、
//! 動画が壊れる。WebM の `SeekPosition` / `CueClusterPosition` も同様に
//! `Segment` 先頭からの相対バイト位置を持つ。
//!
//! 「削除ではなくインプレース無害化」なら、対象領域の前後のバイト位置が
//! 一切動かないため、これらのオフセットテーブルを一切再計算する必要が無い。
//! `free` / `Void` はどちらも規格上「無視してよい」ことが明記された仕組みで、
//! 実運用ツール (`mkvpropedit` 等) も同種の手法を使う。
//!
//! # 対応スコープ
//!
//! - コンテナ: `video/mp4` (ISOBMFF) と `video/webm` (Matroska/EBML) のみ。
//!   マジックバイトで判定し、それ以外は `unsupported_media`。
//! - コーデックは問わない (stream copy のため無関係)。
//! - 寸法 / 再生時間はデコードせずコンテナヘッダから直接読む。取得できない
//!   場合 (想定外バージョン / 壊れたコンテナ) は fail-closed で reject する。
//! - 新規 Cargo 依存は増やさない (hand-roll box / element walker)。

use bytes::Bytes;

use crate::error::ApiError;

/// 出力結果。動画は再エンコードしないため `content_type` は入力コンテナを
/// そのまま反映する (`video/mp4` | `video/webm`)。
#[derive(Debug)]
pub struct ProcessedVideo {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

enum Container {
    Mp4,
    WebM,
}

/// バイト列を受け取り、コンテナメタデータの無害化を行う。
///
/// `max_duration_ms` はコンテナヘッダから読み取った再生時間の上限。超過は
/// reject する (デコード不要でヘッダのみから判定できる)。
pub fn process(input: &[u8], max_duration_ms: u64) -> Result<ProcessedVideo, ApiError> {
    if input.is_empty() {
        return Err(ApiError::bad_request("empty_body", "input bytes are empty"));
    }
    match detect_container(input) {
        Some(Container::Mp4) => mp4::process(input, max_duration_ms),
        Some(Container::WebM) => webm::process(input, max_duration_ms),
        None => Err(ApiError::unsupported_media(
            "unknown_format",
            "could not detect container format from magic bytes",
        )),
    }
}

/// MP4 は `[size(4)][ftyp(4)]...`、`WebM` は EBML header `1A 45 DF A3` から
/// 始まる。どちらも先頭数バイトのマジックバイトで判別できる (decode 不要)。
fn detect_container(input: &[u8]) -> Option<Container> {
    if input.len() >= 8 && &input[4..8] == b"ftyp" {
        return Some(Container::Mp4);
    }
    if input.len() >= 4 && input[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Container::WebM);
    }
    None
}

// ─────────────────────────── MP4 (ISOBMFF) ───────────────────────────

mod mp4 {
    use bytes::Bytes;

    use super::ProcessedVideo;
    use crate::error::ApiError;

    /// 再帰探索の深さ上限。悪意ある深いネストによるスタックオーバーフローを防ぐ。
    /// 実ファイルは `moov > trak > mdia > minf > stbl` 程度 (深さ5) が通常。
    const MAX_DEPTH: u32 = 16;

    /// 中身を掘り下げて `udta` / `meta` / `uuid` を探す対象 (= 規格上コンテナの
    /// ボックス)。ここに無い型は「知らないボックスとして何もせず素通しする」
    /// (= leaf として扱う。`stbl` のようなサンプルテーブル系ボックスも対象外
    /// にすることで実装を単純化している)。
    const CONTAINER_TYPES: &[[u8; 4]] = &[
        *b"moov", *b"trak", *b"mdia", *b"minf", *b"edts", *b"mvex", *b"moof", *b"traf", *b"mfra",
        *b"dinf", *b"meco",
    ];

    /// 無害化 (= `free` に書き換え) する対象ボックス。
    const STRIP_TYPES: &[[u8; 4]] = &[*b"udta", *b"meta", *b"uuid"];

    pub(super) fn process(input: &[u8], max_duration_ms: u64) -> Result<ProcessedVideo, ApiError> {
        let info = extract_info(input)?;
        if info.duration_ms > max_duration_ms {
            return Err(ApiError::too_large(format!(
                "duration {}ms exceeds max {}ms",
                info.duration_ms, max_duration_ms
            )));
        }
        if info.width == 0 || info.height == 0 {
            return Err(ApiError::unsupported_media(
                "mp4_invalid_dims",
                "video track has zero width or height",
            ));
        }

        let mut out = input.to_vec();
        strip_recursive(&mut out, 0)?;

        Ok(ProcessedVideo {
            bytes: Bytes::from(out),
            content_type: "video/mp4",
            width: info.width,
            height: info.height,
            duration_ms: info.duration_ms,
        })
    }

    struct BoxHeader {
        box_type: [u8; 4],
        header_len: usize,
        payload_len: usize,
    }

    /// `buf` 先頭のボックスヘッダを読む。`size == 0` (EOFまで) と `size == 1`
    /// (64bit largesize 拡張) の両方を扱う。壊れたヘッダは fail-closed で
    /// reject する (パニックしない ── 信頼できないバイト列を扱うため)。
    fn read_box_header(buf: &[u8]) -> Result<BoxHeader, ApiError> {
        if buf.len() < 8 {
            return Err(ApiError::unsupported_media(
                "mp4_truncated",
                "box header truncated",
            ));
        }
        let size32 = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let box_type: [u8; 4] = buf[4..8].try_into().unwrap();
        let (header_len, total_len): (usize, u64) = if size32 == 1 {
            if buf.len() < 16 {
                return Err(ApiError::unsupported_media(
                    "mp4_truncated",
                    "largesize box header truncated",
                ));
            }
            let largesize = u64::from_be_bytes(buf[8..16].try_into().unwrap());
            (16, largesize)
        } else if size32 == 0 {
            (8, buf.len() as u64)
        } else {
            (8, u64::from(size32))
        };
        if total_len < header_len as u64 || total_len > buf.len() as u64 {
            return Err(ApiError::unsupported_media(
                "mp4_bad_box_size",
                "box size out of bounds",
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let payload_len = (total_len - header_len as u64) as usize;
        Ok(BoxHeader {
            box_type,
            header_len,
            payload_len,
        })
    }

    /// `buf` (兄弟ボックスが連続する領域) を走査し、`STRIP_TYPES` は
    /// インプレース無害化、`CONTAINER_TYPES` は再帰、それ以外は素通り。
    fn strip_recursive(buf: &mut [u8], depth: u32) -> Result<(), ApiError> {
        if depth > MAX_DEPTH {
            return Err(ApiError::unsupported_media(
                "mp4_too_deep",
                "box nesting exceeds limit",
            ));
        }
        let mut offset = 0usize;
        while offset < buf.len() {
            let header = read_box_header(&buf[offset..])?;
            let total_len = header.header_len + header.payload_len;
            if STRIP_TYPES.contains(&header.box_type) {
                strip_box_in_place(&mut buf[offset..offset + total_len], header.header_len);
            } else if CONTAINER_TYPES.contains(&header.box_type) {
                let payload_start = offset + header.header_len;
                let payload_end = offset + total_len;
                strip_recursive(&mut buf[payload_start..payload_end], depth + 1)?;
            }
            offset += total_len;
        }
        Ok(())
    }

    /// ボックスの type フィールドを `free` に書き換え、payload をゼロ埋め。
    /// `size` フィールド (先頭4バイト、largesize採用時は8バイトの拡張分も)
    /// は一切触らない ── バイト長が変わらないので `free` ボックスとして
    /// そのまま有効。
    fn strip_box_in_place(box_bytes: &mut [u8], header_len: usize) {
        box_bytes[4..8].copy_from_slice(b"free");
        for b in &mut box_bytes[header_len..] {
            *b = 0;
        }
    }

    /// `buf` (兄弟ボックスの連続領域) 内で `want` 型の **最初の1個** の
    /// payload を返す。見つからなければ `Ok(None)`。
    fn find_box(buf: &[u8], want: [u8; 4]) -> Result<Option<&[u8]>, ApiError> {
        let mut offset = 0usize;
        while offset < buf.len() {
            let header = read_box_header(&buf[offset..])?;
            let total_len = header.header_len + header.payload_len;
            if header.box_type == want {
                let payload_start = offset + header.header_len;
                let payload_end = offset + total_len;
                return Ok(Some(&buf[payload_start..payload_end]));
            }
            offset += total_len;
        }
        Ok(None)
    }

    fn find_box_required<'a>(
        buf: &'a [u8],
        want: [u8; 4],
        reason: &'static str,
    ) -> Result<&'a [u8], ApiError> {
        find_box(buf, want)?.ok_or_else(|| {
            ApiError::unsupported_media(
                reason,
                format!("missing required box {:?}", String::from_utf8_lossy(&want)),
            )
        })
    }

    /// `buf` 内の `want` 型ボックスを全て (兄弟レベルのみ、1階層) 集める。
    fn find_all_boxes(buf: &[u8], want: [u8; 4]) -> Result<Vec<&[u8]>, ApiError> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            let header = read_box_header(&buf[offset..])?;
            let total_len = header.header_len + header.payload_len;
            if header.box_type == want {
                let payload_start = offset + header.header_len;
                let payload_end = offset + total_len;
                out.push(&buf[payload_start..payload_end]);
            }
            offset += total_len;
        }
        Ok(out)
    }

    struct Info {
        duration_ms: u64,
        width: u32,
        height: u32,
    }

    fn extract_info(input: &[u8]) -> Result<Info, ApiError> {
        let moov = find_box_required(input, *b"moov", "mp4_no_moov")?;
        let mvhd = find_box_required(moov, *b"mvhd", "mp4_no_mvhd")?;
        let (timescale, duration) = parse_mvhd(mvhd)?;
        let duration_ms = if timescale == 0 {
            0
        } else {
            duration.saturating_mul(1000) / u64::from(timescale)
        };

        let mut dims: Option<(u32, u32)> = None;
        for trak in find_all_boxes(moov, *b"trak")? {
            let mdia = find_box_required(trak, *b"mdia", "mp4_no_mdia")?;
            let hdlr = find_box_required(mdia, *b"hdlr", "mp4_no_hdlr")?;
            let handler_type = parse_hdlr_handler_type(hdlr)?;
            if &handler_type == b"vide" {
                let tkhd = find_box_required(trak, *b"tkhd", "mp4_no_tkhd")?;
                dims = Some(parse_tkhd_dims(tkhd)?);
                break;
            }
        }
        let (width, height) = dims.ok_or_else(|| {
            ApiError::unsupported_media("mp4_no_video_track", "no video track found in moov")
        })?;

        Ok(Info {
            duration_ms,
            width,
            height,
        })
    }

    /// `mvhd` (`FullBox`) から `(timescale, duration)` を読む。version 0/1 で
    /// フィールド幅が異なる。
    fn parse_mvhd(payload: &[u8]) -> Result<(u32, u64), ApiError> {
        if payload.is_empty() {
            return Err(ApiError::unsupported_media(
                "mp4_mvhd_truncated",
                "mvhd payload empty",
            ));
        }
        match payload[0] {
            0 => {
                if payload.len() < 20 {
                    return Err(ApiError::unsupported_media(
                        "mp4_mvhd_truncated",
                        "mvhd v0 payload too short",
                    ));
                }
                let timescale = u32::from_be_bytes(payload[12..16].try_into().unwrap());
                let duration = u32::from_be_bytes(payload[16..20].try_into().unwrap());
                Ok((timescale, u64::from(duration)))
            }
            1 => {
                if payload.len() < 32 {
                    return Err(ApiError::unsupported_media(
                        "mp4_mvhd_truncated",
                        "mvhd v1 payload too short",
                    ));
                }
                let timescale = u32::from_be_bytes(payload[20..24].try_into().unwrap());
                let duration = u64::from_be_bytes(payload[24..32].try_into().unwrap());
                Ok((timescale, duration))
            }
            other => Err(ApiError::unsupported_media(
                "mp4_mvhd_version",
                format!("unsupported mvhd version {other}"),
            )),
        }
    }

    /// `tkhd` (`FullBox`) から `(width, height)` を読む。末尾8バイト
    /// (16.16 固定小数点 x2) が版に依らず幅/高さ。整数部のみ使う。
    fn parse_tkhd_dims(payload: &[u8]) -> Result<(u32, u32), ApiError> {
        if payload.is_empty() {
            return Err(ApiError::unsupported_media(
                "mp4_tkhd_truncated",
                "tkhd payload empty",
            ));
        }
        let (w_off, needed) = match payload[0] {
            0 => (76, 84),
            1 => (88, 96),
            other => {
                return Err(ApiError::unsupported_media(
                    "mp4_tkhd_version",
                    format!("unsupported tkhd version {other}"),
                ));
            }
        };
        if payload.len() < needed {
            return Err(ApiError::unsupported_media(
                "mp4_tkhd_truncated",
                "tkhd payload too short",
            ));
        }
        let width_fixed = u32::from_be_bytes(payload[w_off..w_off + 4].try_into().unwrap());
        let height_fixed = u32::from_be_bytes(payload[w_off + 4..w_off + 8].try_into().unwrap());
        Ok((width_fixed >> 16, height_fixed >> 16))
    }

    /// `hdlr` (`FullBox`) から `handler_type` (4バイト、例 `vide`) を読む。
    fn parse_hdlr_handler_type(payload: &[u8]) -> Result<[u8; 4], ApiError> {
        if payload.len() < 12 {
            return Err(ApiError::unsupported_media(
                "mp4_hdlr_truncated",
                "hdlr payload too short",
            ));
        }
        Ok(payload[8..12].try_into().unwrap())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn build_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
            let total_len = 8 + payload.len();
            let mut out = Vec::with_capacity(total_len);
            #[allow(clippy::cast_possible_truncation)]
            out.extend_from_slice(&(total_len as u32).to_be_bytes());
            out.extend_from_slice(&box_type);
            out.extend_from_slice(payload);
            out
        }

        fn build_mvhd(timescale: u32, duration: u32) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.push(0); // version
            payload.extend_from_slice(&[0, 0, 0]); // flags
            payload.extend_from_slice(&0u32.to_be_bytes()); // creation_time
            payload.extend_from_slice(&0u32.to_be_bytes()); // modification_time
            payload.extend_from_slice(&timescale.to_be_bytes());
            payload.extend_from_slice(&duration.to_be_bytes());
            payload.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
            payload.extend_from_slice(&[0x01, 0x00]); // volume 1.0
            payload.extend_from_slice(&[0, 0]); // reserved
            payload.extend_from_slice(&[0u8; 8]); // reserved
            payload.extend_from_slice(&[0u8; 36]); // matrix
            payload.extend_from_slice(&[0u8; 24]); // pre_defined
            payload.extend_from_slice(&1u32.to_be_bytes()); // next_track_ID
            build_box(*b"mvhd", &payload)
        }

        fn build_tkhd(width: u32, height: u32) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.push(0); // version
            payload.extend_from_slice(&[0, 0, 1]); // flags: track enabled
            payload.extend_from_slice(&0u32.to_be_bytes()); // creation_time
            payload.extend_from_slice(&0u32.to_be_bytes()); // modification_time
            payload.extend_from_slice(&1u32.to_be_bytes()); // track_ID
            payload.extend_from_slice(&0u32.to_be_bytes()); // reserved
            payload.extend_from_slice(&0u32.to_be_bytes()); // duration
            payload.extend_from_slice(&[0u8; 8]); // reserved
            payload.extend_from_slice(&[0, 0]); // layer
            payload.extend_from_slice(&[0, 0]); // alternate_group
            payload.extend_from_slice(&[0, 0]); // volume
            payload.extend_from_slice(&[0, 0]); // reserved
            payload.extend_from_slice(&[0u8; 36]); // matrix
            payload.extend_from_slice(&(width << 16).to_be_bytes());
            payload.extend_from_slice(&(height << 16).to_be_bytes());
            build_box(*b"tkhd", &payload)
        }

        fn build_hdlr(handler_type: [u8; 4]) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.push(0);
            payload.extend_from_slice(&[0, 0, 0]);
            payload.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
            payload.extend_from_slice(&handler_type);
            payload.extend_from_slice(&[0u8; 12]); // reserved
            payload.push(0); // name (empty cstring)
            build_box(*b"hdlr", &payload)
        }

        fn build_mdia(handler_type: [u8; 4]) -> Vec<u8> {
            build_box(*b"mdia", &build_hdlr(handler_type))
        }

        fn build_trak(width: u32, height: u32, handler_type: [u8; 4]) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.extend_from_slice(&build_tkhd(width, height));
            payload.extend_from_slice(&build_mdia(handler_type));
            build_box(*b"trak", &payload)
        }

        fn build_udta_with_gps() -> Vec<u8> {
            build_box(*b"udta", b"FAKE_GPS_DATA_1234567890")
        }

        fn build_moov(timescale: u32, duration: u32, width: u32, height: u32) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.extend_from_slice(&build_mvhd(timescale, duration));
            payload.extend_from_slice(&build_trak(width, height, *b"vide"));
            payload.extend_from_slice(&build_udta_with_gps());
            build_box(*b"moov", &payload)
        }

        fn build_minimal_mp4(
            timescale: u32,
            duration: u32,
            width: u32,
            height: u32,
            mdat_payload: &[u8],
        ) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&build_box(*b"ftyp", b"isomiso2mp41"));
            out.extend_from_slice(&build_moov(timescale, duration, width, height));
            out.extend_from_slice(&build_box(*b"mdat", mdat_payload));
            out
        }

        fn find_top_level_offset(buf: &[u8], want: [u8; 4]) -> Option<usize> {
            let mut offset = 0usize;
            while offset < buf.len() {
                let header = read_box_header(&buf[offset..]).ok()?;
                if header.box_type == want {
                    return Some(offset);
                }
                offset += header.header_len + header.payload_len;
            }
            None
        }

        #[test]
        fn strips_udta_and_preserves_mdat_position_and_content() {
            let mdat_payload = b"FAKE_VIDEO_SAMPLE_DATA_0123456789";
            let input = build_minimal_mp4(1000, 5000, 1920, 1080, mdat_payload);
            let mdat_offset_before =
                find_top_level_offset(&input, *b"mdat").expect("mdat present in fixture");

            let out = process(&input, 60_000).unwrap();

            assert_eq!(out.content_type, "video/mp4");
            assert_eq!(out.width, 1920);
            assert_eq!(out.height, 1080);
            assert_eq!(out.duration_ms, 5000);

            let mdat_offset_after =
                find_top_level_offset(&out.bytes, *b"mdat").expect("mdat still present");
            assert_eq!(
                mdat_offset_before, mdat_offset_after,
                "mdat must not move (offset tables are not recalculated)"
            );
            let sample_start = mdat_offset_after + 8;
            assert_eq!(
                &out.bytes[sample_start..sample_start + mdat_payload.len()],
                mdat_payload,
                "mdat payload must be byte-for-byte unchanged"
            );

            // udta が消え、GPS っぽいペイロードがどこにも残っていない。
            assert!(
                !out.bytes
                    .windows(b"FAKE_GPS_DATA".len())
                    .any(|w| w == b"FAKE_GPS_DATA"),
                "udta payload must be zeroed out"
            );
            // udta は moov 直下にネストされているため、moov のオフセットを
            // 起点に相対探索する。
            let moov_offset =
                find_top_level_offset(&input, *b"moov").expect("moov present in fixture");
            let moov_header = read_box_header(&input[moov_offset..]).unwrap();
            let moov_payload_start = moov_offset + moov_header.header_len;
            let udta_offset_in_moov = find_top_level_offset(&input[moov_payload_start..], *b"udta")
                .expect("udta present in fixture");
            let free_offset = moov_payload_start + udta_offset_in_moov;
            // 同じ絶対オフセットに free ボックスが立っている (位置が動いていない)。
            assert_eq!(&out.bytes[free_offset + 4..free_offset + 8], b"free");
        }

        #[test]
        fn duration_exceeding_max_is_rejected() {
            let input = build_minimal_mp4(1000, 5000, 1920, 1080, b"data");
            let err = process(&input, 1000).unwrap_err();
            assert_eq!(err.reason, "too_large");
        }

        #[test]
        fn missing_video_track_is_rejected() {
            // moov に audio track (handler "soun") のみ。
            let mut payload = Vec::new();
            payload.extend_from_slice(&build_mvhd(1000, 5000));
            payload.extend_from_slice(&build_trak(0, 0, *b"soun"));
            let moov = build_box(*b"moov", &payload);
            let mut input = Vec::new();
            input.extend_from_slice(&build_box(*b"ftyp", b"isom"));
            input.extend_from_slice(&moov);
            input.extend_from_slice(&build_box(*b"mdat", b"data"));

            let err = process(&input, 60_000).unwrap_err();
            assert_eq!(err.reason, "mp4_no_video_track");
        }

        #[test]
        fn mvhd_version1_is_parsed() {
            // version1 mvhd を手組みして timescale/duration の 64bit 幅を検証。
            let mut mvhd_payload = Vec::new();
            mvhd_payload.push(1); // version
            mvhd_payload.extend_from_slice(&[0, 0, 0]); // flags
            mvhd_payload.extend_from_slice(&0u64.to_be_bytes()); // creation_time
            mvhd_payload.extend_from_slice(&0u64.to_be_bytes()); // modification_time
            mvhd_payload.extend_from_slice(&2000u32.to_be_bytes()); // timescale
            mvhd_payload.extend_from_slice(&20000u64.to_be_bytes()); // duration
            mvhd_payload.extend_from_slice(&[0u8; 8]); // reserved
            mvhd_payload.extend_from_slice(&[0, 0]); // layer/alt (unused here, reuse as reserved space)
            mvhd_payload.extend_from_slice(&[0u8; 2]);
            mvhd_payload.extend_from_slice(&[0u8; 36]); // matrix
            mvhd_payload.extend_from_slice(&[0u8; 24]); // pre_defined
            mvhd_payload.extend_from_slice(&1u32.to_be_bytes()); // next_track_ID
            let mvhd = build_box(*b"mvhd", &mvhd_payload);

            let mut moov_payload = Vec::new();
            moov_payload.extend_from_slice(&mvhd);
            moov_payload.extend_from_slice(&build_trak(640, 480, *b"vide"));
            let moov = build_box(*b"moov", &moov_payload);

            let mut input = Vec::new();
            input.extend_from_slice(&build_box(*b"ftyp", b"isom"));
            input.extend_from_slice(&moov);
            input.extend_from_slice(&build_box(*b"mdat", b"data"));

            let out = process(&input, 60_000).unwrap();
            // duration(20000) / timescale(2000) * 1000 = 10000ms
            assert_eq!(out.duration_ms, 10_000);
            assert_eq!(out.width, 640);
            assert_eq!(out.height, 480);
        }

        #[test]
        fn truncated_box_header_is_rejected() {
            let err = process(&[0, 0, 0, 20, b'f', b't', b'y', b'p'], 60_000).unwrap_err();
            assert!(err.reason.starts_with("mp4_"));
        }
    }
}

// ─────────────────────────── WebM (Matroska/EBML) ───────────────────────────

mod webm {
    use bytes::Bytes;

    use super::ProcessedVideo;
    use crate::error::ApiError;

    // EBML ID は最大4バイト。右詰め (leading zero padding) した [u8;4] で
    // 比較する。ID の先頭バイトは marker bit により必ず非ゼロなので、
    // ゼロパディングと実 ID との衝突は起きない。
    const SEGMENT_ID: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
    const INFO_ID: [u8; 4] = [0x15, 0x49, 0xA9, 0x66];
    const TRACKS_ID: [u8; 4] = [0x16, 0x54, 0xAE, 0x6B];
    const TRACK_ENTRY_ID: [u8; 4] = [0x00, 0x00, 0x00, 0xAE];
    const TRACK_TYPE_ID: [u8; 4] = [0x00, 0x00, 0x00, 0x83];
    const VIDEO_ID: [u8; 4] = [0x00, 0x00, 0x00, 0xE0];
    const PIXEL_WIDTH_ID: [u8; 4] = [0x00, 0x00, 0x00, 0xB0];
    const PIXEL_HEIGHT_ID: [u8; 4] = [0x00, 0x00, 0x00, 0xBA];
    const TIMECODE_SCALE_ID: [u8; 4] = [0x00, 0x2A, 0xD7, 0xB1];
    const DURATION_ID: [u8; 4] = [0x00, 0x00, 0x44, 0x89];
    const TAGS_ID: [u8; 4] = [0x12, 0x54, 0xC3, 0x67];
    const ATTACHMENTS_ID: [u8; 4] = [0x19, 0x41, 0xA4, 0x69];
    const CHAPTERS_ID: [u8; 4] = [0x10, 0x43, 0xA7, 0x70];
    const MUXING_APP_ID: [u8; 4] = [0x00, 0x00, 0x4D, 0x80];
    const WRITING_APP_ID: [u8; 4] = [0x00, 0x00, 0x57, 0x41];

    const SEGMENT_STRIP_IDS: &[[u8; 4]] = &[TAGS_ID, ATTACHMENTS_ID, CHAPTERS_ID];
    const INFO_STRIP_IDS: &[[u8; 4]] = &[MUXING_APP_ID, WRITING_APP_ID];

    pub(super) fn process(input: &[u8], max_duration_ms: u64) -> Result<ProcessedVideo, ApiError> {
        let (seg_start, seg_end) = locate_segment(input)?;
        let info = extract_info(&input[seg_start..seg_end])?;
        if info.duration_ms > max_duration_ms {
            return Err(ApiError::too_large(format!(
                "duration {}ms exceeds max {}ms",
                info.duration_ms, max_duration_ms
            )));
        }
        if info.width == 0 || info.height == 0 {
            return Err(ApiError::unsupported_media(
                "webm_invalid_dims",
                "video track has zero width or height",
            ));
        }

        let mut out = input.to_vec();
        strip_children_in_place(&mut out[seg_start..seg_end], SEGMENT_STRIP_IDS)?;
        if let Some((info_start, info_end)) = find_child(&out[seg_start..seg_end], INFO_ID)? {
            strip_children_in_place(
                &mut out[seg_start + info_start..seg_start + info_end],
                INFO_STRIP_IDS,
            )?;
        }

        Ok(ProcessedVideo {
            bytes: Bytes::from(out),
            content_type: "video/webm",
            width: info.width,
            height: info.height,
            duration_ms: info.duration_ms,
        })
    }

    fn vint_len(first_byte: u8) -> Option<usize> {
        if first_byte == 0 {
            return None;
        }
        Some(first_byte.leading_zeros() as usize + 1)
    }

    /// EBML element ID を読む。ID は marker bit を含む値をそのまま比較に使う
    /// (size VINT と異なり ID はマーカービットを剥がさない)。戻り値は右詰め
    /// `[u8;4]` (4バイト超の ID は本実装では非対応、reject する)。
    fn read_element_id(buf: &[u8]) -> Result<([u8; 4], usize), ApiError> {
        if buf.is_empty() {
            return Err(ApiError::unsupported_media(
                "webm_truncated",
                "element id truncated",
            ));
        }
        let len = vint_len(buf[0])
            .ok_or_else(|| ApiError::unsupported_media("webm_bad_id", "invalid EBML id"))?;
        if len > 4 || buf.len() < len {
            return Err(ApiError::unsupported_media(
                "webm_bad_id",
                "EBML id too long or truncated",
            ));
        }
        let mut id = [0u8; 4];
        id[4 - len..].copy_from_slice(&buf[..len]);
        Ok((id, len))
    }

    struct SizeVint {
        value: Option<u64>, // None = 「unknown size」(全 data bit が 1)
        len: usize,
    }

    fn read_size_vint(buf: &[u8]) -> Result<SizeVint, ApiError> {
        if buf.is_empty() {
            return Err(ApiError::unsupported_media(
                "webm_truncated",
                "element size truncated",
            ));
        }
        let len = vint_len(buf[0])
            .ok_or_else(|| ApiError::unsupported_media("webm_bad_size", "invalid EBML size"))?;
        if buf.len() < len {
            return Err(ApiError::unsupported_media(
                "webm_bad_size",
                "EBML size truncated",
            ));
        }
        let marker_bit_pos = 8 - len;
        let first_byte_mask: u8 = if marker_bit_pos >= 8 {
            0
        } else {
            (1u8 << marker_bit_pos) - 1
        };
        let mut value: u64 = u64::from(buf[0] & first_byte_mask);
        for &b in &buf[1..len] {
            value = (value << 8) | u64::from(b);
        }
        let max_value = if len >= 9 {
            u64::MAX
        } else {
            (1u64 << (7 * len)) - 1
        };
        if value == max_value {
            return Ok(SizeVint { value: None, len });
        }
        Ok(SizeVint {
            value: Some(value),
            len,
        })
    }

    /// element の合計バイト長 (`id_len` + `size_len` + `payload_len`) を計算する
    /// 共通ロジック。`unknown size` (= `None`) は `buf` の末尾までを payload
    /// とみなす (本実装では top-level Segment 相当のみ想定)。
    fn element_total_len(buf: &[u8], offset: usize) -> Result<usize, ApiError> {
        let (_, id_len) = read_element_id(&buf[offset..])?;
        let size_vint = read_size_vint(&buf[offset + id_len..])?;
        let header_len = id_len + size_vint.len;
        let payload_len = match size_vint.value {
            Some(v) => usize::try_from(v).map_err(|_| {
                ApiError::unsupported_media("webm_size_overflow", "element size too large")
            })?,
            None => buf.len() - offset - header_len,
        };
        let total_len = header_len + payload_len;
        if offset + total_len > buf.len() {
            return Err(ApiError::unsupported_media(
                "webm_truncated",
                "element extends past buffer",
            ));
        }
        Ok(total_len)
    }

    fn element_header_len(buf: &[u8], offset: usize) -> Result<usize, ApiError> {
        let (_, id_len) = read_element_id(&buf[offset..])?;
        let size_vint = read_size_vint(&buf[offset + id_len..])?;
        Ok(id_len + size_vint.len)
    }

    /// `buf` 先頭から top-level `Segment` 要素の payload 範囲 `(start, end)`
    /// (`buf` 内の絶対 offset) を探す。EBML header 要素は素通りする。
    fn locate_segment(input: &[u8]) -> Result<(usize, usize), ApiError> {
        let mut offset = 0usize;
        while offset < input.len() {
            let (id, _) = read_element_id(&input[offset..])?;
            let header_len = element_header_len(input, offset)?;
            let total_len = element_total_len(input, offset)?;
            if id == SEGMENT_ID {
                return Ok((offset + header_len, offset + total_len));
            }
            offset += total_len;
        }
        Err(ApiError::unsupported_media(
            "webm_no_segment",
            "missing Segment element",
        ))
    }

    /// `buf` 内で `want` 型の最初の直接の子要素の payload 範囲を返す
    /// (`buf` 内の相対 offset)。
    fn find_child(buf: &[u8], want: [u8; 4]) -> Result<Option<(usize, usize)>, ApiError> {
        let mut offset = 0usize;
        while offset < buf.len() {
            let (id, _) = read_element_id(&buf[offset..])?;
            let header_len = element_header_len(buf, offset)?;
            let total_len = element_total_len(buf, offset)?;
            if id == want {
                return Ok(Some((offset + header_len, offset + total_len)));
            }
            offset += total_len;
        }
        Ok(None)
    }

    fn find_all_children(buf: &[u8], want: [u8; 4]) -> Result<Vec<(usize, usize)>, ApiError> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            let (id, _) = read_element_id(&buf[offset..])?;
            let header_len = element_header_len(buf, offset)?;
            let total_len = element_total_len(buf, offset)?;
            if id == want {
                out.push((offset + header_len, offset + total_len));
            }
            offset += total_len;
        }
        Ok(out)
    }

    /// `buf` の直接の子要素を走査し、`strip_ids` に一致する ID を `Void` に
    /// インプレース書き換えする (1階層のみ、再帰しない)。
    fn strip_children_in_place(buf: &mut [u8], strip_ids: &[[u8; 4]]) -> Result<(), ApiError> {
        let mut offset = 0usize;
        while offset < buf.len() {
            let (id, _) = read_element_id(&buf[offset..])?;
            let total_len = element_total_len(buf, offset)?;
            if strip_ids.contains(&id) {
                void_strip_element(&mut buf[offset..offset + total_len]);
            }
            offset += total_len;
        }
        Ok(())
    }

    /// `elem` (ID+size+payload の合計バイト列) を `Void` (1バイトID `0xEC`)
    /// に書き換える。size VINT のバイト長を伸ばして `total_len` を維持する
    /// (= size VINT の非最短エンコードは EBML 仕様上許容される)。
    fn void_strip_element(elem: &mut [u8]) {
        let total_len = elem.len();
        if total_len < 2 {
            // 安全に書き換えるだけの余地が無い (理論上ほぼ発生しない)。
            return;
        }
        let s = (total_len - 1).min(8);
        let p = (total_len - 1 - s) as u64;
        elem[0] = 0xEC; // Void element ID (1 byte)
        write_size_vint(&mut elem[1..=s], p, s);
        for b in &mut elem[1 + s..] {
            *b = 0;
        }
    }

    /// `value` を `len` バイトの size VINT として `dst` に書く。
    /// `value < 2^(7*len)` を呼び出し側で保証すること (`void_strip_element`
    /// の `s` の選び方によりこの不変条件は常に成立する)。
    fn write_size_vint(dst: &mut [u8], value: u64, len: usize) {
        debug_assert!((1..=8).contains(&len));
        debug_assert!(len == 8 || value < (1u64 << (7 * len)));
        for i in 0..len {
            dst[len - 1 - i] = ((value >> (8 * i)) & 0xFF) as u8;
        }
        let marker = 1u8 << (8 - len);
        dst[0] |= marker;
    }

    fn parse_uint(buf: &[u8]) -> Result<u64, ApiError> {
        if buf.len() > 8 {
            return Err(ApiError::unsupported_media(
                "webm_uint_too_long",
                "uint element too long",
            ));
        }
        let mut v: u64 = 0;
        for &b in buf {
            v = (v << 8) | u64::from(b);
        }
        Ok(v)
    }

    fn parse_float(buf: &[u8]) -> Result<f64, ApiError> {
        match buf.len() {
            4 => Ok(f64::from(f32::from_be_bytes(buf.try_into().unwrap()))),
            8 => Ok(f64::from_be_bytes(buf.try_into().unwrap())),
            _ => Err(ApiError::unsupported_media(
                "webm_float_bad_len",
                "float element has unexpected length",
            )),
        }
    }

    struct Info {
        duration_ms: u64,
        width: u32,
        height: u32,
    }

    fn extract_info(segment: &[u8]) -> Result<Info, ApiError> {
        let (info_start, info_end) = find_child(segment, INFO_ID)?
            .ok_or_else(|| ApiError::unsupported_media("webm_no_info", "missing Info element"))?;
        let info_buf = &segment[info_start..info_end];

        let timecode_scale = match find_child(info_buf, TIMECODE_SCALE_ID)? {
            Some((s, e)) => parse_uint(&info_buf[s..e])?,
            None => 1_000_000, // spec 既定値 (ns)
        };
        let (ds, de) = find_child(info_buf, DURATION_ID)?.ok_or_else(|| {
            ApiError::unsupported_media("webm_no_duration", "missing Duration element")
        })?;
        let duration_raw = parse_float(&info_buf[ds..de])?;
        // `TimecodeScale` は仕様上 u32 相当のナノ秒単位の値 (既定 1_000_000)
        // で、動画の再生時間計算に使う程度の精度で f64 への変換ロスは
        // 実用上問題にならない。
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let duration_ms = ((duration_raw * timecode_scale as f64) / 1_000_000.0) as u64;

        let (tracks_start, tracks_end) = find_child(segment, TRACKS_ID)?.ok_or_else(|| {
            ApiError::unsupported_media("webm_no_tracks", "missing Tracks element")
        })?;
        let tracks_buf = &segment[tracks_start..tracks_end];

        let mut dims: Option<(u32, u32)> = None;
        for (te_s, te_e) in find_all_children(tracks_buf, TRACK_ENTRY_ID)? {
            let entry = &tracks_buf[te_s..te_e];
            let track_type = find_child(entry, TRACK_TYPE_ID)?
                .map(|(s, e)| parse_uint(&entry[s..e]))
                .transpose()?;
            if track_type != Some(1) {
                continue; // 1 = video (Matroska TrackType 仕様)
            }
            if let Some((vs, ve)) = find_child(entry, VIDEO_ID)? {
                let video_buf = &entry[vs..ve];
                let width = find_child(video_buf, PIXEL_WIDTH_ID)?
                    .map(|(s, e)| parse_uint(&video_buf[s..e]))
                    .transpose()?;
                let height = find_child(video_buf, PIXEL_HEIGHT_ID)?
                    .map(|(s, e)| parse_uint(&video_buf[s..e]))
                    .transpose()?;
                #[allow(clippy::cast_possible_truncation)]
                if let (Some(w), Some(h)) = (width, height) {
                    dims = Some((w as u32, h as u32));
                    break;
                }
            }
        }
        let (width, height) = dims.ok_or_else(|| {
            ApiError::unsupported_media("webm_no_video_track", "no video track found")
        })?;

        Ok(Info {
            duration_ms,
            width,
            height,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn build_element(id: &[u8], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(id);
            let mut size_bytes = [0u8; 8];
            write_size_vint(&mut size_bytes, payload.len() as u64, 8);
            out.extend_from_slice(&size_bytes);
            out.extend_from_slice(payload);
            out
        }

        const EBML_HEADER_ID: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
        const SEGMENT_ID_BYTES: &[u8] = &[0x18, 0x53, 0x80, 0x67];
        const INFO_ID_BYTES: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
        const TIMECODE_SCALE_ID_BYTES: &[u8] = &[0x2A, 0xD7, 0xB1];
        const DURATION_ID_BYTES: &[u8] = &[0x44, 0x89];
        const TRACKS_ID_BYTES: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
        const TRACK_ENTRY_ID_BYTES: &[u8] = &[0xAE];
        const TRACK_TYPE_ID_BYTES: &[u8] = &[0x83];
        const VIDEO_ID_BYTES: &[u8] = &[0xE0];
        const PIXEL_WIDTH_ID_BYTES: &[u8] = &[0xB0];
        const PIXEL_HEIGHT_ID_BYTES: &[u8] = &[0xBA];
        const TAGS_ID_BYTES: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
        const CLUSTER_ID_BYTES: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];

        fn build_video_entry(width: u32, height: u32) -> Vec<u8> {
            let mut video_payload = Vec::new();
            video_payload.extend_from_slice(&build_element(
                PIXEL_WIDTH_ID_BYTES,
                &u64::from(width).to_be_bytes()[6..],
            ));
            video_payload.extend_from_slice(&build_element(
                PIXEL_HEIGHT_ID_BYTES,
                &u64::from(height).to_be_bytes()[6..],
            ));
            let video = build_element(VIDEO_ID_BYTES, &video_payload);

            let mut entry_payload = Vec::new();
            entry_payload.extend_from_slice(&build_element(TRACK_TYPE_ID_BYTES, &[1]));
            entry_payload.extend_from_slice(&video);
            build_element(TRACK_ENTRY_ID_BYTES, &entry_payload)
        }

        fn build_minimal_webm(
            timecode_scale: u32,
            duration: f64,
            width: u32,
            height: u32,
            cluster_payload: &[u8],
            tags_payload: &[u8],
        ) -> Vec<u8> {
            let mut info_payload = Vec::new();
            info_payload.extend_from_slice(&build_element(
                TIMECODE_SCALE_ID_BYTES,
                &(u64::from(timecode_scale)).to_be_bytes()[5..],
            ));
            info_payload
                .extend_from_slice(&build_element(DURATION_ID_BYTES, &duration.to_be_bytes()));
            let info = build_element(INFO_ID_BYTES, &info_payload);

            let tracks_payload = build_video_entry(width, height);
            let tracks = build_element(TRACKS_ID_BYTES, &tracks_payload);

            let cluster = build_element(CLUSTER_ID_BYTES, cluster_payload);
            let tags = build_element(TAGS_ID_BYTES, tags_payload);

            let mut segment_payload = Vec::new();
            segment_payload.extend_from_slice(&info);
            segment_payload.extend_from_slice(&tracks);
            segment_payload.extend_from_slice(&cluster);
            segment_payload.extend_from_slice(&tags);
            let segment = build_element(SEGMENT_ID_BYTES, &segment_payload);

            let mut out = Vec::new();
            out.extend_from_slice(&build_element(EBML_HEADER_ID, b"\x01"));
            out.extend_from_slice(&segment);
            out
        }

        fn find_top_level_offset(buf: &[u8], want: &[u8]) -> Option<usize> {
            let mut offset = 0usize;
            while offset < buf.len() {
                let (id, id_len) = read_element_id(&buf[offset..]).ok()?;
                let header_len = element_header_len(buf, offset).ok()?;
                let total_len = element_total_len(buf, offset).ok()?;
                let want_padded = {
                    let mut w = [0u8; 4];
                    w[4 - want.len()..].copy_from_slice(want);
                    w
                };
                if id == want_padded {
                    return Some(offset);
                }
                let _ = (id_len, header_len);
                offset += total_len;
            }
            None
        }

        #[test]
        fn strips_tags_and_preserves_cluster_position_and_content() {
            let cluster_payload = b"FAKE_CLUSTER_FRAME_DATA_0123456789";
            let tags_payload = b"FAKE_TAG_METADATA_ABCDEFGHIJ";
            let input =
                build_minimal_webm(1_000_000, 3_000.0, 1280, 720, cluster_payload, tags_payload);

            // Segment 内での Cluster の絶対オフセットを事前に記録。
            let (seg_start, seg_end) = locate_segment(&input).unwrap();
            let cluster_offset_before =
                find_top_level_offset(&input[seg_start..seg_end], CLUSTER_ID_BYTES)
                    .expect("cluster present in fixture");

            let out = process(&input, 60_000).unwrap();

            assert_eq!(out.content_type, "video/webm");
            assert_eq!(out.width, 1280);
            assert_eq!(out.height, 720);
            // duration 3000.0 * timecode_scale(1_000_000ns) / 1_000_000 = 3_000_000ms... 実際は
            // duration(3000.0) はTimecodeScale単位のカウントなので 3000 * 1_000_000ns = 3s = 3000ms。
            assert_eq!(out.duration_ms, 3_000);

            let (out_seg_start, out_seg_end) = locate_segment(&out.bytes).unwrap();
            let cluster_offset_after =
                find_top_level_offset(&out.bytes[out_seg_start..out_seg_end], CLUSTER_ID_BYTES)
                    .expect("cluster still present");
            assert_eq!(
                seg_start + cluster_offset_before,
                out_seg_start + cluster_offset_after,
                "cluster must not move"
            );

            assert!(
                !out.bytes
                    .windows(b"FAKE_TAG_METADATA".len())
                    .any(|w| w == b"FAKE_TAG_METADATA"),
                "tags payload must be zeroed out"
            );
            let tags_offset =
                find_top_level_offset(&input[seg_start..seg_end], TAGS_ID_BYTES).unwrap();
            assert_eq!(
                out.bytes[seg_start + tags_offset],
                0xEC,
                "Tags id must become Void"
            );
        }

        #[test]
        fn duration_exceeding_max_is_rejected() {
            let input = build_minimal_webm(1_000_000, 3_000.0, 1280, 720, b"data", b"tags");
            let err = process(&input, 1000).unwrap_err();
            assert_eq!(err.reason, "too_large");
        }

        #[test]
        fn missing_info_is_rejected() {
            let segment_payload = build_element(TRACKS_ID_BYTES, &build_video_entry(640, 480));
            let segment = build_element(SEGMENT_ID_BYTES, &segment_payload);
            let mut input = Vec::new();
            input.extend_from_slice(&build_element(EBML_HEADER_ID, b"\x01"));
            input.extend_from_slice(&segment);

            let err = process(&input, 60_000).unwrap_err();
            assert_eq!(err.reason, "webm_no_info");
        }

        #[test]
        fn void_strip_element_round_trips() {
            let mut elem = build_element(TAGS_ID_BYTES, b"some tag payload data here");
            let total_len = elem.len();
            void_strip_element(&mut elem);
            assert_eq!(elem.len(), total_len, "total length must be unchanged");
            assert_eq!(elem[0], 0xEC);
            // 書き換え後を size VINT として再パースし、payload が全部0で
            // total_len と整合すること。
            let (id, id_len) = read_element_id(&elem).unwrap();
            assert_eq!(id, [0, 0, 0, 0xEC]);
            let size = read_size_vint(&elem[id_len..]).unwrap();
            let payload_start = id_len + size.len;
            let payload_len = usize::try_from(size.value.unwrap()).unwrap();
            assert_eq!(payload_start + payload_len, total_len);
            assert!(elem[payload_start..].iter().all(|&b| b == 0));
        }
    }
}
