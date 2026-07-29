//! リモートカスタム絵文字の学習ロジック (Issue #328 系フォローアップ)。
//!
//! 元々は `dispatch/reaction.rs` に「reaction content と一致する Emoji tag を
//! 1 件だけ学習する」形で実装されていたが、**Note 本文中の Emoji tag は
//! 全件学習する必要がある**(投稿本文に複数のカスタム絵文字が使われうるため、
//! content 一致という前提が成り立たない)ため、1 個の Emoji tag オブジェクトを
//! 検証・学習する最小単位([`learn_emoji_tag_object`])をここに切り出し、
//! reaction 用 (`dispatch/reaction.rs::learn_emoji_tag`、1 件一致)と
//! Note 用 ([`learn_note_emoji_tags`]、全件)の両方から呼べるようにする。
//!
//! ## 学習元
//!
//! - リアクション受信 (`Like`/`EmojiReact`) の `tag` フィールド。
//! - Note 受信 (`Create`/`Update`、Announce 経由の未知 Note fetch-store 含む)
//!   の `tag` フィールド。
//!
//! ## 信頼境界
//!
//! `Emoji.id` の host は呼び出し元が渡す `signer_host` (= reaction の場合は
//! リアクションした人、Note の場合は投稿者の AP host) と一致することを要求
//! する ── 他インスタンスの絵文字 ID を spoofing 学習させない (identity 境界)。
//! `Emoji.icon.url` の host は signer と異なってよい (Issue #239: Misskey は
//! drive/画像を別ドメインで配信する)。画像取得は media-proxy が SSRF 境界を
//! 担い、client へは自鯖キャッシュ URL を返すため任意 host を許容できる。

use std::time::Duration;

use anyhow::Context;
use aws_sdk_s3::primitives::ByteStream;
use chrono::Utc;
use sakurasato_core::model::EmojiRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{info, warn};
use url::Url;

use crate::state::AppState;

/// remote emoji を media-proxy 経由で取得してキャッシュするときの variant
/// 文字列 (= `emoji_import.rs::EMOJI_VARIANT` と同値)。media-proxy 側の
/// `Variant::Emoji` (512x512 / WebP 単一フレーム or animated) と揃える ──
/// `crates/media-proxy/src/image_pipeline.rs` を参照。
const EMOJI_VARIANT: &str = "emoji";

/// 取得済み remote emoji を versitygw に置く prefix。`routes/media.rs` の
/// 許可リスト ([`crate::routes::media::REMOTE_EMOJI_KEY_PREFIX`]) と同期
/// させる ── 名前を grep で揃えやすいよう同 prefix を使う。
const REMOTE_EMOJI_KEY_PREFIX: &str = "emoji/remote/";

/// remote emoji の fetch が失敗してから再試行するまでの最短間隔。相手サーバ
/// が恒常的に落ちている / 4xx を返している場合に毎回 fetch が走らないように
/// backoff する。固定 1h で開始 ── exponential 化は future issue。
const FAILURE_RETRY_AFTER: Duration = Duration::from_hours(1);

/// 学習に成功したリモート custom emoji の識別情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LearnedEmoji {
    /// `emoji` テーブルの id (= `reaction.emoji_id` に入れる)。
    pub(crate) id: i64,
    /// 学習した絵文字の host (= 検証済み signer host、lowercase)。
    pub(crate) host: String,
}

/// tag が `{"type":"Emoji", ...}` かどうか。大文字小文字は区別しない
/// (連合先実装のばらつきに寛容にする、既存 `learn_emoji_tag` の判定を踏襲)。
pub(crate) fn is_emoji_tag(tag: &JsonValue) -> bool {
    tag.get("type")
        .and_then(JsonValue::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("Emoji"))
}

/// `ActorRow.ap_id` (または Note author の `ap_id`) から検証済み lowercase
/// host を取り出す。
pub(crate) fn host_from_ap_id(ap_id: &str) -> Option<String> {
    Some(Url::parse(ap_id).ok()?.host_str()?.to_lowercase())
}

/// `:foo:` / `:foo@host:` の `foo` 部分だけを返す。`:` で挟まれていない場合は
/// `None` (= Unicode emoji)。
pub(crate) fn extract_shortcode(content: &str) -> Option<&str> {
    let stripped = content.strip_prefix(':')?.strip_suffix(':')?;
    // `:foo@host:` 形式から host を落とす。
    let shortcode = stripped.split('@').next()?;
    if shortcode.is_empty() {
        None
    } else {
        Some(shortcode)
    }
}

/// 1 個の Emoji tag オブジェクトを検証・学習する最小単位。
///
/// `tag`は`is_emoji_tag`で`type=="Emoji"`と判定済みのものを渡す前提
/// (本関数内では`type`は再チェックしない)。`signer_host`と`Emoji.id`の
/// host が一致しないものは spoofing とみなし拒否する。
///
/// **戻り値の意味**: media-proxy fetch が失敗しても (`image_key=None` で
/// upsert される)`Some`を返す ── DBには行が作られている(未キャッシュ状態)
/// ため。`None`になるのは tag 形式不正・shortcode charset 不正・`Emoji.id`
/// host 不一致・`icon.url` 欠如/不正・`upsert_remote`自体の失敗のときのみ。
#[allow(
    clippy::too_many_lines,
    reason = "reaction.rs から移設した single straight-line 検証+fetch+upsert"
)]
pub(crate) async fn learn_emoji_tag_object(
    state: &AppState,
    signer_host: &str,
    tag: &JsonValue,
) -> Option<LearnedEmoji> {
    let obj = tag.as_object()?;
    let name = obj.get("name").and_then(JsonValue::as_str)?;
    let shortcode = extract_shortcode(name)?;

    // PR #191 round-1 ⚠️ #1: `extract_shortcode` は `:` を剥がして `@` で
    // 分割するだけで charset を見ない。`/` 入りの malicious shortcode が
    // versitygw に `emoji/remote/<host>/a/b.webp` で書かれて namespace を
    // 汚染しないよう、fetch + PUT の前で `is_valid_shortcode` を強制する。
    if !repo::emoji::is_valid_shortcode(shortcode) {
        warn!(
            shortcode,
            "Emoji.name shortcode failed charset validation; refusing to learn"
        );
        return None;
    }

    let ap_id = obj.get("id").and_then(JsonValue::as_str)?;
    // Emoji.id の host が signer host と一致しなければ拒否 (spoofing)。
    let Ok(emoji_url) = Url::parse(ap_id) else {
        return None;
    };
    let emoji_host = emoji_url.host_str()?;
    if !emoji_host.eq_ignore_ascii_case(signer_host) {
        warn!(
            emoji_id = ap_id,
            signer_host, emoji_host, "Emoji.id host mismatch with signer; refusing to learn"
        );
        return None;
    }

    let icon = obj.get("icon").and_then(JsonValue::as_object);
    let image_url = icon
        .and_then(|i| i.get("url"))
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let media_type = icon
        .and_then(|i| i.get("mediaType"))
        .and_then(JsonValue::as_str)
        .unwrap_or("application/octet-stream");

    if image_url.is_empty() {
        return None;
    }
    // Issue #239: icon.url の host == signer host 検査は行わない。Misskey は
    // emoji メタデータと drive/画像を別ドメインで配信するのが一般的なため。
    // ここでは media-proxy に渡す前の最低限の well-formedness のみ要求する。
    let url_well_formed = Url::parse(image_url)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some());
    if !url_well_formed {
        warn!(
            emoji_id = ap_id,
            image_url, "Emoji.icon.url is not a well-formed http(s) URL; refusing to learn"
        );
        return None;
    }

    // 既存 row を見て fetch をスキップできるか判定する。
    let existing = repo::emoji::get_by_ap_id(state.pool(), ap_id)
        .await
        .inspect_err(|err| {
            warn!(?err, emoji_id = ap_id, "get_by_ap_id failed; refetching");
        })
        .ok()
        .flatten();

    if let Some(ref row) = existing
        && let Some(reason) = should_skip_fetch(row)
    {
        info!(
            emoji_id = row.id,
            shortcode,
            host = signer_host,
            reason,
            "remote emoji fetch skipped (cache hit / recent failure backoff)",
        );
        return Some(LearnedEmoji {
            id: row.id,
            host: signer_host.to_string(),
        });
    }

    // fetch を試みる。失敗時は `image_key = None` を SQL 側 COALESCE で温存
    // させ、`last_failed_at = now()` で TTL backoff の起点にする。
    let (image_key_for_upsert, stored_media_type, last_failed_at) =
        match fetch_and_cache_remote_emoji(state, image_url, signer_host, shortcode).await {
            Ok((key, mt)) => (Some(key), mt, None),
            Err(err) => {
                warn!(
                    ?err,
                    emoji_id = ap_id,
                    image_url,
                    "remote emoji fetch/cache failed; recording last_failed_at backoff"
                );
                (None, media_type.to_string(), Some(Utc::now()))
            }
        };

    let new = repo::emoji::NewRemoteEmoji {
        shortcode: shortcode.to_string(),
        ap_id: ap_id.to_string(),
        host: signer_host.to_string(),
        image_key: image_key_for_upsert,
        media_type: stored_media_type,
        last_failed_at,
    };
    match repo::emoji::upsert_remote(state.pool(), new).await {
        Ok(row) => {
            info!(
                emoji_id = row.id,
                shortcode,
                host = signer_host,
                image_cached = row.image_key.is_some(),
                last_failed_at = ?row.last_failed_at,
                "remote emoji learned",
            );
            Some(LearnedEmoji {
                id: row.id,
                host: signer_host.to_string(),
            })
        }
        Err(err) => {
            warn!(?err, emoji_id = ap_id, "remote emoji upsert failed");
            None
        }
    }
}

/// 既存 emoji row を見て fetch (= media-proxy → versitygw PUT) をスキップする
/// 判定。`Some(reason)` を返したら [`learn_emoji_tag_object`] は即座に
/// `row.id` を返す。
///
/// スキップ条件:
/// 1. `image_key` が `emoji/remote/` prefix を持つ ── 自鯖に焼き済みなので
///    内容を再取得する必要はない。
/// 2. `last_failed_at` が直近 [`FAILURE_RETRY_AFTER`] 以内 ── 失敗 backoff。
///    相手サーバが落ちている / 4xx を返している期間に毎回 fetch を叩かない。
fn should_skip_fetch(existing: &EmojiRow) -> Option<&'static str> {
    if existing
        .image_key
        .as_deref()
        .is_some_and(|k| k.starts_with(REMOTE_EMOJI_KEY_PREFIX))
    {
        return Some("cache_hit");
    }
    if let Some(failed_at) = existing.last_failed_at {
        let elapsed = Utc::now().signed_duration_since(failed_at).to_std().ok();
        if elapsed.is_some_and(|e| e < FAILURE_RETRY_AFTER) {
            return Some("recent_failure_backoff");
        }
    }
    None
}

/// remote emoji の画像を media-proxy 経由で取得し、versitygw に格納する。
/// 成功時は `(versitygw_key, "image/webp")` を返す。失敗時は anyhow エラーを
/// 返し、呼び出し側で `image_key = None` を選ばせる。
///
/// **副作用**: versitygw 上に `emoji/remote/<host>/<shortcode>.webp` を PUT
/// する。同名 key への複数回 PUT は idempotent (= 上書き) なので、cache hit
/// 判定で弾けなかった経路で重複 PUT が走っても害は無い。
async fn fetch_and_cache_remote_emoji(
    state: &AppState,
    image_url: &str,
    signer_host: &str,
    shortcode: &str,
) -> anyhow::Result<(String, String)> {
    let processed = state
        .media_proxy()
        .fetch_image(image_url, EMOJI_VARIANT)
        .await
        .with_context(|| format!("media-proxy fetch {image_url}"))?;

    // host / shortcode は事前に検証済み (signer_host=正規化済 hostname、
    // shortcode=`[a-zA-Z0-9_-]{1,128}`)。path traversal にならない。
    let storage_key = format!("{REMOTE_EMOJI_KEY_PREFIX}{signer_host}/{shortcode}.webp");
    let media_type = processed.content_type.clone();
    state
        .s3_client()
        .put_object()
        .bucket(&state.config().storage.bucket)
        .key(&storage_key)
        .content_type(&media_type)
        .body(ByteStream::from(processed.bytes))
        .send()
        .await
        .with_context(|| format!("versitygw PUT {storage_key}"))?;
    Ok((storage_key, media_type))
}

/// Note (Create/Update/Announce fetch-store 共通) の `tag` 配列から remote
/// custom emoji を **全件** 学習する。reaction 用 (`dispatch/reaction.rs::
/// learn_emoji_tag`) と異なり content との shortcode 一致は見ない ──
/// 投稿本文には複数のカスタム絵文字が使われうるため。
///
/// `author_ap_id`の host 抽出に失敗したら (= 不正な URL 等) 即 default
/// (silent no-op)。`tags`が配列でなければ同様に no-op。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NoteEmojiLearnSummary {
    pub(crate) emoji_tags_seen: usize,
    pub(crate) emoji_learned: usize,
}

pub(crate) async fn learn_note_emoji_tags(
    state: &AppState,
    author_ap_id: &str,
    tags: &JsonValue,
) -> NoteEmojiLearnSummary {
    let mut summary = NoteEmojiLearnSummary::default();
    let Some(author_host) = host_from_ap_id(author_ap_id) else {
        return summary;
    };
    let Some(tag_array) = tags.as_array() else {
        return summary;
    };
    for tag in tag_array {
        if !is_emoji_tag(tag) {
            continue;
        }
        summary.emoji_tags_seen += 1;
        if learn_emoji_tag_object(state, &author_host, tag)
            .await
            .is_some()
        {
            summary.emoji_learned += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_shortcode_strips_colons_and_host() {
        assert_eq!(extract_shortcode(":blob:"), Some("blob"));
        assert_eq!(extract_shortcode(":blob_party:"), Some("blob_party"));
        assert_eq!(extract_shortcode(":blob@misskey.io:"), Some("blob"));
        assert_eq!(extract_shortcode("👍"), None);
        assert_eq!(extract_shortcode(""), None);
        assert_eq!(extract_shortcode(":"), None);
        assert_eq!(extract_shortcode("::"), None);
        assert_eq!(extract_shortcode(":@host:"), None);
    }

    /// `extract_shortcode` 自身は charset を見ない。`/` 入りの shortcode を
    /// Some で返してしまうため、後段の `is_valid_shortcode` が弾く責務を持つ
    /// ── 本テストはその境界仕様を固定する。
    #[test]
    fn extract_shortcode_does_not_filter_charset() {
        assert_eq!(extract_shortcode(":a/b:"), Some("a/b"));
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode("a/b"));
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode(
            "../escape"
        ));
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode(""));
        assert!(sakurasato_core::repo::emoji::is_valid_shortcode("blob"));
        assert!(sakurasato_core::repo::emoji::is_valid_shortcode(
            "blob_party-1"
        ));
    }

    #[test]
    fn is_emoji_tag_matches_case_insensitively() {
        assert!(is_emoji_tag(&serde_json::json!({"type": "Emoji"})));
        assert!(is_emoji_tag(&serde_json::json!({"type": "emoji"})));
        assert!(!is_emoji_tag(&serde_json::json!({"type": "Hashtag"})));
        assert!(!is_emoji_tag(&serde_json::json!({})));
    }

    #[test]
    fn host_from_ap_id_lowercases_host() {
        assert_eq!(
            host_from_ap_id("https://Misskey.IO/emojis/blob"),
            Some("misskey.io".to_string())
        );
        assert_eq!(host_from_ap_id("not a url"), None);
    }

    // ── should_skip_fetch のテスト ────────────────────────────

    fn emoji_row_fixture(
        image_key: Option<&str>,
        last_failed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> EmojiRow {
        EmojiRow {
            id: 1,
            shortcode: "blob".into(),
            host: Some("remote.test".into()),
            category: None,
            aliases: sqlx::types::Json(vec![]),
            image_key: image_key.map(str::to_string),
            media_type: "image/webp".into(),
            ap_id: Some("https://remote.test/emojis/blob".into()),
            is_local: false,
            license: None,
            is_sensitive: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_failed_at,
        }
    }

    #[test]
    fn should_skip_fetch_returns_cache_hit_for_remote_prefix() {
        let row = emoji_row_fixture(Some("emoji/remote/remote.test/blob.webp"), None);
        assert_eq!(should_skip_fetch(&row), Some("cache_hit"));
    }

    #[test]
    fn should_skip_fetch_returns_none_for_legacy_url() {
        // 旧 URL row は cache hit でも recent failure でもないので fetch を試みる。
        let row = emoji_row_fixture(Some("https://remote.test/files/blob.png"), None);
        assert!(should_skip_fetch(&row).is_none());
    }

    #[test]
    fn should_skip_fetch_returns_backoff_for_recent_failure() {
        // 直近 (30 min 前) に失敗 → backoff TTL (1h) 内なので skip。
        let recent =
            chrono::Utc::now() - chrono::Duration::from_std(Duration::from_mins(30)).unwrap();
        let row = emoji_row_fixture(None, Some(recent));
        assert_eq!(should_skip_fetch(&row), Some("recent_failure_backoff"));
    }

    #[test]
    fn should_skip_fetch_returns_none_after_backoff_window() {
        // TTL (1h) を超えた失敗 → retry を許す。
        let old = chrono::Utc::now() - chrono::Duration::from_std(Duration::from_hours(2)).unwrap();
        let row = emoji_row_fixture(None, Some(old));
        assert!(should_skip_fetch(&row).is_none());
    }

    #[test]
    fn should_skip_fetch_returns_none_for_fresh_row() {
        // image_key=None かつ last_failed_at=None (= 新規 row 直前) は fetch を試みる。
        let row = emoji_row_fixture(None, None);
        assert!(should_skip_fetch(&row).is_none());
    }

    /// 旧 URL row + 直近失敗 → backoff で skip。COALESCE 保存とあわせて
    /// 「旧 URL が残ったまま再 fetch も控える」状態を実現する。
    #[test]
    fn should_skip_fetch_combines_legacy_url_and_recent_failure() {
        let recent =
            chrono::Utc::now() - chrono::Duration::from_std(Duration::from_mins(1)).unwrap();
        let row = emoji_row_fixture(Some("https://remote.test/files/blob.png"), Some(recent));
        assert_eq!(should_skip_fetch(&row), Some("recent_failure_backoff"));
    }
}
