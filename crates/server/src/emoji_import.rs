//! `sakurasato emoji import <zip>` — Misskey 形式の絵文字 zip を取り込む (M8 PR1)。
//!
//! ## ファイル形式 (CLAUDE.md §5.4)
//!
//! トップレベル `meta.json` に以下のスキーマが入る:
//!
//! ```jsonc
//! {
//!   "metaVersion": 2,                  // 我々は 1 / 2 両方受ける
//!   "host": "misskey.io",              // export 元 (ログ目的のみ)
//!   "exportedAt": "2025-...",         // RFC3339 (ログ目的のみ)
//!   "emojis": [
//!     {
//!       "downloaded": true,            // false は zip 内に画像が無い → 取込まない
//!       "fileName": "emojis/foo.png",  // zip 内のエントリ名
//!       "emoji": {
//!         "name": "foo",               // shortcode (`:foo:`)
//!         "category": "blob",
//!         "aliases": ["bar"]
//!       }
//!     }, ...
//!   ]
//! }
//! ```
//!
//! ## 取込フロー
//!
//! 1. zip を `std::fs::File` で開き、`zip::ZipArchive` を構築。
//! 2. `meta.json` を読み、サイズ上限 [`META_MAX_BYTES`] を超えていれば拒否。
//! 3. `emojis[]` のうち `downloaded == true` のものだけ列挙し、shortcode を
//!    [`repo::emoji::is_valid_shortcode`] で検査 (= `[a-zA-Z0-9_-]{1,128}`、
//!    Issue #188 で 64 → 128 緩和、Misskey に揃え)。
//! 4. 各エントリの画像バイト列を **本体ではデコードせず**、`media-proxy` の
//!    `/v1/image/sanitize?variant=emoji` に流す。返ってきた WebP を versitygw
//!    に `emoji/local/<shortcode>.webp` で PUT、`emoji` 行を upsert (同 shortcode
//!    は上書き、CLAUDE.md §5.4 仕様)。
//! 5. 1 件ずつ処理 → 失敗は warn でログに残しスキップ、次のエントリへ。最後に
//!    成功 / スキップ / 失敗の件数を出力する。
//!
//! ## 防御
//!
//! - **zip-slip**: `fileName` は `zip::ZipArchive::by_name` で参照されるが、
//!   versitygw に書き込むキーは **shortcode 由来** (`emoji/local/<shortcode>.webp`)
//!   で完全に決まる ── fileName を偽装してもストレージのパスは汚染されない。
//! - **decompression bomb**: 各エントリの解凍前サイズを [`MAX_IMAGE_BYTES`] で
//!   切り、ストリーミング受信側でも `Read::take` で重ねて切る。
//! - **エントリ数上限**: [`MAX_EMOJIS`] (10,000) を超えるリクエストは拒否。
//! - **画像デコード**: 本モジュールは `image` クレートを **引かない**。
//!   サニタイズ済みバイト列だけが versitygw に上がる。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek};

use anyhow::{Context, anyhow, bail};
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use sakurasato_core::{Config, repo};
use serde::Deserialize;
use tracing::{info, warn};

use crate::cli::{EmojiArgs, EmojiCommand, EmojiImportArgs};
use crate::state::AppState;

/// `meta.json` のサイズ上限 (1 MiB)。Misskey の export は通常 KB オーダで、
/// MB 級は事実上ありえない ── 巨大化は decompression-bomb 系の攻撃と見なす。
const META_MAX_BYTES: u64 = 1024 * 1024;

/// 1 絵文字あたりの画像サイズ上限 (2 MiB)。Misskey は 256 KiB 前後が典型値。
const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

/// 1 zip あたりの emojis 件数上限。お一人様サーバの現実的な上限。
const MAX_EMOJIS: usize = 10_000;

/// media-proxy `/v1/image/sanitize` に渡す variant 文字列。
const EMOJI_VARIANT: &str = "emoji";

/// versitygw 上の local emoji オブジェクトキープレフィックス。
/// 既存 actor avatars (`<hex>.webp` 直下) と衝突しないよう別空間に置く。
const LOCAL_EMOJI_KEY_PREFIX: &str = "emoji/local/";

#[derive(Debug, Deserialize)]
struct MisskeyMeta {
    /// 1 (古い meta) / 2 (現行) を観測しているが、未知バージョンも warn して受ける。
    #[serde(default, rename = "metaVersion")]
    meta_version: Option<u32>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default, rename = "exportedAt")]
    exported_at: Option<String>,
    emojis: Vec<MisskeyEmojiEntry>,
}

#[derive(Debug, Deserialize)]
struct MisskeyEmojiEntry {
    /// false の場合は zip に画像が同梱されていない (元インスタンスの DB 上
    /// だけにある参照) → 取り込まない。
    #[serde(default)]
    downloaded: bool,
    #[serde(default, rename = "fileName")]
    file_name: Option<String>,
    emoji: MisskeyEmojiBody,
}

#[derive(Debug, Deserialize)]
struct MisskeyEmojiBody {
    /// `:foo:` の `foo` 部分 (コロン無し)。
    name: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    /// Misskey export の license (= 利用条件 free text)。古い / 他実装の zip では
    /// 欠落しうるので `default`。
    #[serde(default)]
    license: Option<String>,
    /// Misskey `isSensitive`。欠落時は `false`。
    #[serde(default, rename = "isSensitive")]
    is_sensitive: bool,
}

/// 1 zip 全体の処理結果サマリ。CLI 出力 / テスト assertion 用。
/// `Serialize` はローカル API (`POST /api/v1/emojis/import`) が JSON として
/// そのまま返すため (Issue #328 系)。
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportSummary {
    /// 成功して DB upsert + versitygw PUT まで完了した件数。
    pub imported: usize,
    /// `downloaded == false` で取り込まなかった件数。
    pub skipped_not_downloaded: usize,
    /// shortcode 不正・fileName 欠如・サイズ超過などで skip した件数。
    pub skipped_invalid: usize,
    /// media-proxy または versitygw エラーで失敗した件数。
    pub failed: usize,
}

/// `sakurasato emoji <subcommand>` のエントリポイント。
pub async fn run(config: Config, args: EmojiArgs) -> anyhow::Result<()> {
    match args.command {
        EmojiCommand::Import(import) => run_import(config, import).await,
        EmojiCommand::BackfillRemote => crate::emoji_backfill::run(config).await,
    }
}

async fn run_import(config: Config, args: EmojiImportArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let file =
        File::open(&args.zip).with_context(|| format!("open emoji zip {}", args.zip.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("parse zip {}", args.zip.display()))?;
    let summary = import_archive(&state, &mut archive).await?;
    info!(
        imported = summary.imported,
        skipped_not_downloaded = summary.skipped_not_downloaded,
        skipped_invalid = summary.skipped_invalid,
        failed = summary.failed,
        "emoji import complete",
    );
    println!(
        "imported {} emojis ({} skipped (not downloaded), {} skipped (invalid), {} failed)",
        summary.imported, summary.skipped_not_downloaded, summary.skipped_invalid, summary.failed,
    );
    if summary.failed > 0 {
        bail!("{} emoji(s) failed during import; see logs", summary.failed);
    }
    Ok(())
}

/// テスト・本番共通の主処理。`state` から media-proxy / DB / S3 client を引き、
/// 任意の `Read + Seek` ベースの zip を 1 つ処理する。
///
/// `pub(crate)`: CLI (`run_import`) に加え、ローカル API
/// (`local_api::emoji_admin::import`) からも同じロジックを呼ぶ (Issue #328 系)。
#[allow(
    clippy::too_many_lines,
    reason = "single straight-line loop over emoji entries"
)]
pub(crate) async fn import_archive<R: Read + Seek>(
    state: &AppState,
    archive: &mut zip::ZipArchive<R>,
) -> anyhow::Result<ImportSummary> {
    let meta = read_meta(archive).context("read meta.json from zip")?;
    info!(
        meta_version = meta.meta_version.unwrap_or(0),
        host = meta.host.as_deref().unwrap_or("?"),
        exported_at = meta.exported_at.as_deref().unwrap_or("?"),
        emoji_count = meta.emojis.len(),
        "emoji zip metadata",
    );
    if meta.emojis.len() > MAX_EMOJIS {
        bail!(
            "emoji zip contains {} entries (> {} cap); refusing to import",
            meta.emojis.len(),
            MAX_EMOJIS,
        );
    }

    let bucket = state.config().storage.bucket.clone();
    let mut summary = ImportSummary::default();
    let mut seen_shortcodes: BTreeSet<String> = BTreeSet::new();

    for entry in meta.emojis {
        if !entry.downloaded {
            summary.skipped_not_downloaded += 1;
            continue;
        }
        let Some(file_name) = entry.file_name.as_deref() else {
            warn!(shortcode = %entry.emoji.name, "skipping emoji: missing fileName");
            summary.skipped_invalid += 1;
            continue;
        };
        let shortcode = entry.emoji.name.clone();
        if !repo::emoji::is_valid_shortcode(&shortcode) {
            warn!(shortcode = %shortcode, "skipping emoji: shortcode rejected by regex");
            summary.skipped_invalid += 1;
            continue;
        }
        // zip 内に同一 shortcode が複数あった場合、最初のひとつだけ処理する。
        // Misskey export では通常起きないが、攻撃的に同名重複を仕込まれても
        // 1 回 PUT + 1 回 upsert で抑える。
        if !seen_shortcodes.insert(shortcode.clone()) {
            warn!(
                shortcode = %shortcode,
                "skipping emoji: duplicate shortcode in zip"
            );
            summary.skipped_invalid += 1;
            continue;
        }

        let bytes = match read_emoji_bytes(archive, file_name) {
            Ok(b) => b,
            Err(err) => {
                warn!(
                    shortcode = %shortcode,
                    file_name = %file_name,
                    error = %err,
                    "skipping emoji: failed to read bytes from zip"
                );
                summary.skipped_invalid += 1;
                continue;
            }
        };

        let processed = match state
            .media_proxy()
            .sanitize_image(bytes, EMOJI_VARIANT)
            .await
        {
            Ok(p) => p,
            Err(err) => {
                warn!(
                    shortcode = %shortcode,
                    error = %err,
                    "emoji sanitize failed; skipping"
                );
                summary.failed += 1;
                continue;
            }
        };

        let storage_key = format!("{LOCAL_EMOJI_KEY_PREFIX}{shortcode}.webp");
        // `processed.bytes` は `bytes::Bytes` で ref-counted。`ByteStream::from`
        // が `Bytes` を直接受け取れるので `.to_vec()` のコピーは省ける
        // ([[m8-pr1-review]] PR #44 軽微指摘 #1)。
        let media_type = processed.content_type;
        let put = state
            .s3_client()
            .put_object()
            .bucket(&bucket)
            .key(&storage_key)
            .content_type(&media_type)
            .body(ByteStream::from(processed.bytes))
            .send()
            .await;
        if let Err(err) = put {
            warn!(
                shortcode = %shortcode,
                storage_key = %storage_key,
                error = ?err,
                "versitygw PUT failed; skipping"
            );
            summary.failed += 1;
            continue;
        }

        let new = repo::emoji::NewLocalEmoji {
            shortcode: shortcode.clone(),
            category: entry.emoji.category.clone(),
            aliases: entry.emoji.aliases.clone(),
            image_key: storage_key.clone(),
            media_type: media_type.clone(),
            license: entry.emoji.license.clone(),
            is_sensitive: entry.emoji.is_sensitive,
        };
        if let Err(err) = repo::emoji::upsert_local(state.pool(), new).await {
            warn!(
                shortcode = %shortcode,
                error = ?err,
                "emoji DB upsert failed; skipping"
            );
            summary.failed += 1;
            continue;
        }
        summary.imported += 1;
    }

    Ok(summary)
}

/// `meta.json` を読む。サイズ上限 [`META_MAX_BYTES`] を超えていれば拒否。
///
/// `ZipFile::size()` (= zip ヘッダの宣言値) と `Read::take` の二重ガードで
/// 切る ── `read_emoji_bytes` と対称。ヘッダが `uncompressed_size=0` と
/// 嘘をついても実際の inflate を上限で打ち切る ([[m8-pr1-review]] PR #44)。
fn read_meta<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> anyhow::Result<MisskeyMeta> {
    let file = archive
        .by_name("meta.json")
        .map_err(|e| anyhow!("meta.json not found in zip: {e}"))?;
    if file.size() > META_MAX_BYTES {
        bail!(
            "meta.json is {} bytes (> {} cap); refusing to parse",
            file.size(),
            META_MAX_BYTES,
        );
    }
    let cap = usize::try_from(file.size()).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    let mut limited = file.take(META_MAX_BYTES + 1);
    limited
        .read_to_end(&mut buf)
        .context("read meta.json bytes")?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > META_MAX_BYTES {
        bail!("meta.json exceeded {META_MAX_BYTES} bytes during inflate");
    }
    serde_json::from_slice(&buf).context("parse meta.json")
}

/// `file_name` で指定されたエントリのバイト列を読み出す。
/// 解凍前に `ZipFile::size()` を [`MAX_IMAGE_BYTES`] で切り、解凍中も
/// `Read::take` で重ねて切る (zip header の uncompressed size を信用しない)。
fn read_emoji_bytes<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> anyhow::Result<Bytes> {
    let file = archive
        .by_name(file_name)
        .map_err(|e| anyhow!("entry {file_name:?} not found in zip: {e}"))?;
    if file.size() > MAX_IMAGE_BYTES {
        bail!(
            "emoji entry {file_name:?} is {} bytes (> {} cap)",
            file.size(),
            MAX_IMAGE_BYTES,
        );
    }
    let cap = usize::try_from(file.size()).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    let mut limited = file.take(MAX_IMAGE_BYTES + 1);
    limited
        .read_to_end(&mut buf)
        .with_context(|| format!("read emoji bytes from {file_name:?}"))?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_IMAGE_BYTES {
        bail!("emoji entry {file_name:?} exceeded {MAX_IMAGE_BYTES} bytes during inflate");
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// `MisskeyMeta` のスキーマ受容性。
    #[test]
    fn meta_accepts_v2_schema() {
        let raw = r#"{
            "metaVersion": 2,
            "host": "misskey.io",
            "exportedAt": "2025-01-02T03:04:05Z",
            "emojis": [
                {"downloaded": true, "fileName": "emojis/blob.png",
                 "emoji": {"name": "blob_party", "category": "blob", "aliases": ["party"]}},
                {"downloaded": false,
                 "emoji": {"name": "missing", "aliases": []}}
            ]
        }"#;
        let meta: MisskeyMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(meta.meta_version, Some(2));
        assert_eq!(meta.emojis.len(), 2);
        assert!(meta.emojis[0].downloaded);
        assert_eq!(meta.emojis[0].emoji.name, "blob_party");
        assert!(!meta.emojis[1].downloaded);
    }

    /// `license` / `isSensitive` を meta.json から取り込む (round-trip export 用)。
    /// 欠落時は `None` / `false` に倒れる (古い / 他実装の zip 寛容性)。
    #[test]
    fn meta_captures_license_and_sensitive() {
        let raw = r#"{
            "metaVersion": 2,
            "emojis": [
                {"downloaded": true, "fileName": "a.png",
                 "emoji": {"name": "licensed", "license": "CC-BY-4.0", "isSensitive": true}},
                {"downloaded": true, "fileName": "b.png",
                 "emoji": {"name": "plain"}}
            ]
        }"#;
        let meta: MisskeyMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(meta.emojis[0].emoji.license.as_deref(), Some("CC-BY-4.0"));
        assert!(meta.emojis[0].emoji.is_sensitive);
        // 欠落は default。
        assert!(meta.emojis[1].emoji.license.is_none());
        assert!(!meta.emojis[1].emoji.is_sensitive);
    }

    /// 古い metaVersion=1 (= host / exportedAt が無いケース) も受ける。
    #[test]
    fn meta_accepts_v1_partial_schema() {
        let raw = r#"{
            "metaVersion": 1,
            "emojis": [
                {"downloaded": true, "fileName": "blob.png",
                 "emoji": {"name": "blob"}}
            ]
        }"#;
        let meta: MisskeyMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(meta.meta_version, Some(1));
        assert!(meta.host.is_none());
        assert_eq!(meta.emojis.len(), 1);
        assert!(meta.emojis[0].emoji.category.is_none());
        assert!(meta.emojis[0].emoji.aliases.is_empty());
    }

    /// metaVersion 欠如でもパースは通す (未知 export に対する寛容性)。
    #[test]
    fn meta_accepts_missing_version() {
        let raw = r#"{"emojis": []}"#;
        let meta: MisskeyMeta = serde_json::from_str(raw).unwrap();
        assert!(meta.meta_version.is_none());
        assert!(meta.emojis.is_empty());
    }

    /// メモリ上に最小 zip を組み立てるヘルパ。
    fn build_zip(meta_json: &str, extras: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("meta.json", opts).unwrap();
            zw.write_all(meta_json.as_bytes()).unwrap();
            for (name, bytes) in extras {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    /// `read_meta` がアーカイブから JSON を読めること (Cursor で代用)。
    #[test]
    fn read_meta_round_trip() {
        let raw = r#"{"metaVersion":2,"emojis":[]}"#;
        let zip_bytes = build_zip(raw, &[]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let parsed = read_meta(&mut archive).unwrap();
        assert_eq!(parsed.meta_version, Some(2));
        assert!(parsed.emojis.is_empty());
    }

    /// `read_meta` が `META_MAX_BYTES` を超える meta.json を拒否する。
    #[test]
    fn read_meta_rejects_oversized() {
        let big = format!(
            "{{\"emojis\":[], \"_pad\":\"{}\"}}",
            "a".repeat(usize::try_from(META_MAX_BYTES).unwrap() + 16)
        );
        let zip_bytes = build_zip(&big, &[]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let err = read_meta(&mut archive).unwrap_err();
        assert!(format!("{err}").contains("meta.json is"));
    }

    /// `take` ガードの追加で、`META_MAX_BYTES` 直下のサイズは引き続き受ける
    /// ことを確認 (回帰防止: take limit を `META_MAX_BYTES` ぴったりに置くと
    /// off-by-one でちょうど境界の合法 zip を拒否してしまう)。
    #[test]
    fn read_meta_accepts_at_limit() {
        // `_pad` の中身を調整して meta.json 全体が META_MAX_BYTES 未満になるよう詰める。
        // 余裕を持って 32 byte 引いておく ── JSON 構造のオーバーヘッド分。
        let pad_len = usize::try_from(META_MAX_BYTES).unwrap() - 32;
        let near_limit = format!("{{\"emojis\":[], \"_pad\":\"{}\"}}", "a".repeat(pad_len));
        assert!(u64::try_from(near_limit.len()).unwrap() < META_MAX_BYTES);
        let zip_bytes = build_zip(&near_limit, &[]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let meta = read_meta(&mut archive).expect("just below limit must be accepted");
        assert!(meta.emojis.is_empty());
    }

    /// `read_emoji_bytes` が解凍前のヘッダサイズで弾くこと。
    #[test]
    fn read_emoji_bytes_rejects_oversized_header() {
        let raw = r#"{"emojis":[]}"#;
        let big = vec![0u8; usize::try_from(MAX_IMAGE_BYTES).unwrap() + 1];
        let zip_bytes = build_zip(raw, &[("big.bin", &big)]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let err = read_emoji_bytes(&mut archive, "big.bin").unwrap_err();
        assert!(format!("{err}").contains("> "));
    }

    /// `read_emoji_bytes` が読み出した bytes を返すこと。
    #[test]
    fn read_emoji_bytes_returns_payload() {
        let raw = r#"{"emojis":[]}"#;
        let payload = b"hello-emoji-bytes";
        let zip_bytes = build_zip(raw, &[("hi.png", payload)]);
        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let got = read_emoji_bytes(&mut archive, "hi.png").unwrap();
        assert_eq!(got.as_ref(), payload);
    }

    /// shortcode validation を通した invalid 入力が `is_valid_shortcode` で
    /// reject されること (= 取込時に `skipped_invalid` に倒れる経路の核)。
    #[test]
    fn invalid_shortcodes_are_rejected() {
        for bad in ["../escape", "with space", "コロン", "", "x:y"] {
            assert!(
                !repo::emoji::is_valid_shortcode(bad),
                "expected {bad:?} to be rejected"
            );
        }
        for ok in ["blob_party", "tada", "x-y", "A0"] {
            assert!(
                repo::emoji::is_valid_shortcode(ok),
                "expected {ok:?} to pass"
            );
        }
    }

    /// Issue #188: 長さ上限を 64 → 128 に緩和した境界回帰テスト。
    /// 1 / 64 / 128 が通り、0 / 129 が拒否される。
    #[test]
    fn shortcode_length_boundary_at_128() {
        // 旧上限 (64) を超える 65〜128 char の shortcode は受理されるように。
        assert!(repo::emoji::is_valid_shortcode("a"));
        assert!(repo::emoji::is_valid_shortcode(&"a".repeat(64)));
        assert!(repo::emoji::is_valid_shortcode(&"a".repeat(65)));
        assert!(repo::emoji::is_valid_shortcode(&"a".repeat(128)));
        // 0 / 129 は引き続き拒否。
        assert!(!repo::emoji::is_valid_shortcode(""));
        assert!(!repo::emoji::is_valid_shortcode(&"a".repeat(129)));
        // 文字種制約は変えていないので、64 chars までだった旧挙動の
        // 文字種違反 (空白 / 全角等) は引き続き拒否される。
        assert!(!repo::emoji::is_valid_shortcode(&"a ".repeat(64)));
    }
}
