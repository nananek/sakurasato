//! `GET /api/v1/timeline/home` ── ホームタイムライン取得。
//!
//! 自分の投稿 + accepted follow している remote actor の投稿を、`published_at`
//! 降順 (= 新しい順) で返す。
//!
//! 自分 / followee の note に加え、自分 / followee の **renote (boost)** を
//! `published_at` で混ぜて返す (= renote エントリは元 note + renoter 情報を持つ)。
//!
//! ## クエリパラメータ
//!
//! - `limit` (任意、既定 40、上限 80) ── 1 回で返す件数。
//! - `before_ts_ms` (任意) ── epoch ミリ秒。これより前の note / renote を返す。
//!   note と renote は id 連番が別々なので時刻カーソルで混在ページングする。
//!   `TimelineResponse.next_before_ts_ms` をそのまま渡す。
//!
//! ## エラー
//!
//! - 503: ローカル actor 未 init / DB アクセス失敗。
//!   フォロー先 actor が居ない初期状態でも 200 + 空配列を返す (自分の投稿が
//!   無くてもエラーにしない)。

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use sakurasato_core::repo::note::TimelineEntry;
use sakurasato_core::repo::reaction::ReactionSummaryRow;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::local_api::media::build_media_url;
use crate::state::AppState;

pub(crate) const LIMIT_DEFAULT: i64 = 40;
pub(crate) const LIMIT_MAX: i64 = 80;

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    /// 次ページのカーソル。**epoch ミリ秒**で、これより前
    /// (`published_at` がこの時刻より小さい) の note / renote を返す。note と
    /// renote は id 連番が別々なので、id カーソルではなく `published_at` 一本で
    /// 混在ページングする (= `MiAuth` タイムラインと同じ方式)。整数なのでクエリ
    /// エンコードの曖昧さが無い。`TimelineResponse.next_before_ts_ms` をそのまま
    /// 渡せばよい。
    #[serde(default)]
    pub before_ts_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TimelineNote {
    pub id: i64,
    pub ap_id: String,
    pub url: Option<String>,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    pub actor_display_name: Option<String>,
    /// 投稿主のアバター URL。M5 PR2 で TUI 側が画像表示に使う。
    /// クライアントが直接 fetch する想定 (server は decode しない)。
    pub actor_icon_url: Option<String>,
    pub content: String,
    pub summary: Option<String>,
    pub language: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub is_local: bool,
    /// M8 PR3: 受領したリアクション集計 (`content` 単位)。空 Vec は省略しない
    /// (= 必ず `reactions: []` を返す) ── 既存 TUI の serde は配列 default が
    /// `Vec::new()` で安全。
    #[serde(default)]
    pub reactions: Vec<ReactionSummaryDto>,
    /// Issue #133 (4): 添付メディア。AP `Document` を扱いやすい形に正規化
    /// した一覧。Timeline の `📎 N` バッジ件数と、Note 詳細モーダルの
    /// プレビューに使う。空 Vec は省略しない (= 必ず `attachments: []`)。
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    /// Issue #133 (5): 本文の `:shortcode:` に対応する Emoji tag 一覧
    /// (AP `tag` のうち `type == "Emoji"` だけ抜き出した形)。詳細モーダルで
    /// shortcode と画像のギャラリー表示に使う。空 Vec は省略しない。
    #[serde(default)]
    pub emojis: Vec<EmojiDto>,
    /// #151: この Note が何回 boost / renote されたか (受信 + 送出側の合計)。
    /// `announce` テーブル `count(*)` 由来。
    #[serde(default)]
    pub announce_count: i64,
    /// #151: viewer (= ローカル actor) 自身が renote 済みか。TUI の「↻ you
    /// renoted」マーカー表示に使う。
    #[serde(default)]
    pub viewer_renoted: bool,
    /// このエントリが **renote (boost) として流れてきた** 場合の付帯情報。
    /// `Some` のとき、本 `TimelineNote` の本体フィールド (author / content /
    /// reactions …) は **元 note** を表し、`renote` が「誰がいつ renote したか」
    /// を持つ。TUI は `renote.is_some()` で「🔁 <renoter> がリノート」ヘッダを
    /// 出して元 note を描画する。通常の note では `None` (= 省略)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renote: Option<RenoteMeta>,
}

/// renote (boost / announce) として流れてきたエントリの「誰が renote したか」。
/// 本体の `TimelineNote` は元 note を表し、こちらが renoter と announce を指す。
#[derive(Debug, Clone, Serialize)]
pub struct RenoteMeta {
    /// `announce` 行 id (= renote の取り消し等で参照)。
    pub announce_id: i64,
    pub announce_ap_id: String,
    pub renoter_actor_id: i64,
    pub renoter_ap_id: String,
    pub renoter_preferred_username: String,
    pub renoter_display_name: Option<String>,
    pub renoter_icon_url: Option<String>,
    /// renote した時刻 (= timeline 上の並び位置)。
    pub renoted_at: DateTime<Utc>,
}

/// Note 添付の TUI 向け正規化形式。AP `Document` / `Image` のフィールドの
/// うち TUI が実描画に使う部分だけを引き出す。
///
/// - `url`: 表示用 URL (= `/media/proxy?url=...` 経由で fetch する元 URL)。
///   ローカル添付は `https://<host>/media/<key>`、リモートは送られてきた URL。
/// - `media_type`: `image/webp` 等。`image/` で始まらない (= 動画など) なら
///   TUI は preview をスキップしてリンクだけ出す。
/// - `alt`: AP `name` 由来の代替テキスト。なければ `None`。
/// - `width` / `height`: 元 Document の寸法 (= AP では任意)。preview のアスペクト比に。
#[derive(Debug, Serialize)]
pub struct AttachmentDto {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Note 本文中で参照される custom emoji の最小情報。`shortcode` は AP
/// `name` で `:foo:` (ローカル) または `:foo@host:` (リモート) の形を維持。
///
/// `image_url` は媒体取得用 ── ローカル emoji は `/media/emoji/local/...`、
/// リモートは AP `icon.url` を素のまま渡す (= TUI 側は `media/proxy?url=`
/// 経由で fetch)。
#[derive(Debug, Serialize)]
pub struct EmojiDto {
    pub shortcode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// `Some(true)` = ローカル絵文字 (= `/media/proxy?url=` 経由で OK)、
    /// `Some(false)` = リモート、`None` = 由来不明 (= `image_url` の host を
    /// 自インスタンスと比較する)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_local: Option<bool>,
}

/// `TimelineNote.reactions` の 1 要素。`content` は AP のまま (`:foo:` /
/// Unicode / `:foo@host:`)。`emoji_image_url` は local emoji の場合
/// `/media/emoji/local/...` の絶対 URL、remote emoji の場合は連合先サーバ
/// の URL がそのまま入る (TUI は `media/proxy?url=...&variant=emoji` 経由で
/// fetch する想定)。
#[derive(Debug, Clone, Serialize)]
pub struct ReactionSummaryDto {
    pub content: String,
    pub count: i64,
    #[serde(default)]
    pub emoji_image_url: Option<String>,
    #[serde(default)]
    pub emoji_media_type: Option<String>,
    /// `Some(true)` = local emoji (= 直接 `image_url` を fetch してよい)、
    /// `Some(false)` = remote emoji (= TUI 側で proxy 経由)、`None` = Unicode。
    #[serde(default)]
    pub emoji_is_local: Option<bool>,
}

impl TimelineNote {
    /// `TimelineEntry` + 集約データ (reactions / announce) を 1 個の DTO に
    /// 組み立てる。`announce` が `None` のときは「集計取得に失敗」 or
    /// 「該当 row 無し」のどちらでも安全側に `count = 0, viewer_renoted = false`
    /// で返す ── タイムライン本体は表示し続けたい。
    /// `e` は **元 note** (renote エントリでは renote 元、通常エントリでは note
    /// 本体)。`renote` が `Some` のとき「これは renote として流れてきた」を表す。
    /// note window と renote window で同じ note を別エントリとして 2 度出すこと
    /// があるので `&TimelineEntry` を借用で受け、フィールドは clone する。
    pub(crate) fn from_entry_with_aggregates(
        e: &TimelineEntry,
        reactions: Vec<ReactionSummaryDto>,
        announce: Option<&sakurasato_core::repo::announce::AnnounceSummaryRow>,
        host: &str,
        renote: Option<RenoteMeta>,
    ) -> Self {
        let attachments = parse_attachments(&e.attachments);
        let emojis = parse_emojis(&e.tags, host);
        let (announce_count, viewer_renoted) =
            announce.map_or((0, false), |a| (a.count, a.viewer_renoted));
        Self {
            id: e.id,
            ap_id: e.ap_id.clone(),
            url: e.url.clone(),
            actor_id: e.actor_id,
            actor_ap_id: e.actor_ap_id.clone(),
            actor_preferred_username: e.actor_preferred_username.clone(),
            actor_display_name: e.actor_display_name.clone(),
            actor_icon_url: e.actor_icon_url.clone(),
            content: e.content.clone(),
            summary: e.summary.clone(),
            language: e.language.clone(),
            visibility: e.visibility.clone(),
            sensitive: e.sensitive,
            in_reply_to_ap_id: e.in_reply_to_ap_id.clone(),
            in_reply_to_note_id: e.in_reply_to_note_id,
            published_at: e.published_at,
            is_local: e.is_local,
            reactions,
            attachments,
            emojis,
            announce_count,
            viewer_renoted,
            renote,
        }
    }
}

/// `note.attachments` JSONB を [`AttachmentDto`] の Vec に正規化する。AP
/// `Document` / `Image` / `Audio` / `Video` を `url` / `mediaType` / `name`
/// / `width` / `height` だけ抜く。`url` を持たない要素は無視する。
pub(crate) fn parse_attachments(raw: &JsonValue) -> Vec<AttachmentDto> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            // server 側で URL のスキームを `http(s)://` に絞る ── TUI 側
            // `image_cache::vet_url` でも同様のチェックがあるが、ここで
            // 落とすことで `file://` / `javascript:` / `data:` 等が API
            // レスポンス JSON に乗ること自体を防ぐ (= 多層防御)。
            let url = v
                .get("url")
                .and_then(JsonValue::as_str)
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                .filter(|u| u.len() <= URL_MAX_BYTES)?
                .to_string();
            let media_type = v
                .get("mediaType")
                .and_then(JsonValue::as_str)
                .filter(|s| s.len() <= MEDIA_TYPE_MAX_BYTES)
                .map(str::to_string);
            let alt = v
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty() && s.len() <= ATTACHMENT_ALT_MAX_BYTES)
                .map(str::to_string);
            let width = v.get("width").and_then(JsonValue::as_u64).and_then(|w| {
                if w > u64::from(u32::MAX) {
                    None
                } else {
                    u32::try_from(w).ok()
                }
            });
            let height = v.get("height").and_then(JsonValue::as_u64).and_then(|h| {
                if h > u64::from(u32::MAX) {
                    None
                } else {
                    u32::try_from(h).ok()
                }
            });
            Some(AttachmentDto {
                url,
                media_type,
                alt,
                width,
                height,
            })
        })
        .take(ATTACHMENTS_PER_NOTE_MAX)
        .collect()
}

/// shortcode (= AP `Emoji.name`) の**文字数**上限。AP には明示の規約が無いが
/// Mastodon は 50 文字未満、Misskey は 100 文字程度を想定している ── 連合
/// 先が極端に長い文字列を送り込むと TUI レンダリングで Line span が膨らみ
/// レイアウト計算に響くため、防御的に 128 で切る。`str::len()` (= byte) では
/// なく `chars().count()` で測ることで、CJK 文字 (1 文字 3 byte) でも文字数
/// として 128 まで通る (= 識別子としての意味で 128 文字、UTF-8 byte で 384)。
const SHORTCODE_MAX_CHARS: usize = 128;

/// 1 Note あたりの添付件数上限。Mastodon は 4 件、Misskey も 16 件程度が
/// 通常で、これを超える Note は実用上ない。連合先が 10,000 件の添付を
/// 送り込んで TUI メモリ / Line span を肥大化させるのを防ぐ防御層。
const ATTACHMENTS_PER_NOTE_MAX: usize = 32;

/// 1 Note あたりの emoji 件数上限。連合先からの `DoS` 風入力を弾く防御層。
/// Mastodon / Misskey の通常 Note では 数〜十数件が上限なので余裕を持たせて 128。
const EMOJIS_PER_NOTE_MAX: usize = 128;

/// 添付 alt text のバイト長上限。AP `name` は本来サイズ制約が無いため、
/// 連合先が極端に長い文字列を送ってきても TUI Span が爆発しないよう截る。
/// 識別子ではなく説明文なので chars ではなく byte で十分 (= UTF-8 boundary は
/// 別途、保存時に保証されている前提)。
const ATTACHMENT_ALT_MAX_BYTES: usize = 1500;

/// `mediaType` 文字列の上限。実用的な MIME type は 100 byte 以内に収まる。
const MEDIA_TYPE_MAX_BYTES: usize = 100;

/// 添付 / 絵文字 `url` のバイト長上限。実用 URL は数百 byte で十分で、AP
/// 仕様上の URI 制約も同程度。連合先が ~900 KB の URL 文字列を送り込んで
/// API レスポンスと TUI heap を肥大化させる `DoS` 入力を弾く。
const URL_MAX_BYTES: usize = 2048;

/// `note.tags` JSONB を走査し `type == "Emoji"` の要素だけ [`EmojiDto`] に
/// 変換する。AP `Emoji` は `name` (shortcode) と `icon.url` を持つ。
///
/// `is_local` は `image_url` の host を `local_host` (= 自インスタンス) と
/// 比較して決める。AP の `Emoji` 自体には `is_local` フィールドが無いため
/// host 比較が現状唯一の信号。`local_host` に port が混じっていても合致
/// するよう、両辺をパースして `host_str()` 同士で比較する。
pub(crate) fn parse_emojis(raw: &JsonValue, local_host: &str) -> Vec<EmojiDto> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    let local_normalized = normalize_host_for_compare(local_host);
    arr.iter()
        .filter_map(|v| {
            if v.get("type").and_then(JsonValue::as_str) != Some("Emoji") {
                return None;
            }
            let shortcode = v
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .filter(|s| s.chars().count() <= SHORTCODE_MAX_CHARS)?
                .to_string();
            let icon = v.get("icon");
            // round-3 review Finding 2: `parse_attachments` と同じく
            // server 側で URL スキームを `http(s)://` に絞る。TUI 側
            // `vet_url` も落とすが、API レスポンス JSON に乗ること自体を
            // 防ぐ多層防御 (= 一貫性ある方針)。
            let image_url = icon
                .and_then(|i| i.get("url"))
                .and_then(JsonValue::as_str)
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                .filter(|u| u.len() <= URL_MAX_BYTES)
                .map(str::to_string);
            let media_type = icon
                .and_then(|i| i.get("mediaType"))
                .and_then(JsonValue::as_str)
                .filter(|s| s.len() <= MEDIA_TYPE_MAX_BYTES)
                .map(str::to_string);
            let is_local = image_url
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|p| p.host_str().map(str::to_ascii_lowercase))
                .map(|h| h == local_normalized);
            Some(EmojiDto {
                shortcode,
                image_url,
                media_type,
                is_local,
            })
        })
        .take(EMOJIS_PER_NOTE_MAX)
        .collect()
}

/// `local_host` 設定値を `host_str` 比較用に正規化する。`config.server.host`
/// は通常 `"example.com"` だが、開発環境で `"example.com:8443"` のように
/// port が付くことがある ── [`url::Url`] パースを試み、`host_str()` のみを
/// 取り出して lowercase 化する。パース失敗時 (= スキームなし純粋ホスト名)
/// は素のままを lowercase 化する。
fn normalize_host_for_compare(s: &str) -> String {
    if let Some(host) = url::Url::parse(&format!("https://{s}"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    {
        return host;
    }
    s.to_ascii_lowercase()
}

/// `ReactionSummaryRow` (DB) → `ReactionSummaryDto` (API)。
///
/// `image_key` の解釈 (Issue #135 で remote も自鯖キャッシュに移行):
/// - `emoji/local/<shortcode>.webp` → 自鯖 `/media/...` URL に展開
/// - `emoji/remote/<host>/<shortcode>.webp` → 同じく自鯖 `/media/...` URL
/// - `https://...` / `http://...` (= 旧 row の remote pass-through) → 素通し。
///   再 upsert で `emoji/remote/...` に書き換わるまでの graceful migration。
/// - `None` (= remote fetch 失敗の placeholder) → `emoji_image_url = None`
pub(crate) fn row_to_dto(host: &str, row: ReactionSummaryRow) -> ReactionSummaryDto {
    let emoji_image_url = row.image_key.as_deref().map(|key| {
        if key.starts_with("https://") || key.starts_with("http://") {
            key.to_string()
        } else {
            build_media_url(host, key)
        }
    });
    ReactionSummaryDto {
        content: row.content,
        count: row.count,
        emoji_image_url,
        emoji_media_type: row.media_type,
        emoji_is_local: row.is_local,
    }
}

#[derive(Debug, Serialize)]
pub struct TimelineResponse {
    pub notes: Vec<TimelineNote>,
    /// 次ページを取るときに渡す `before_ts_ms` (= 最後のエントリの並び時刻を
    /// epoch ミリ秒にしたもの。note なら `published_at`、renote なら renote 時刻)。
    /// `notes` が空のとき `None`。`TimelineQuery.before_ts_ms` にそのまま渡す。
    pub next_before_ts_ms: Option<i64>,
}

#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "timeline merge を 1 関数で組む / renoted・renoter は AP 用語"
)]
pub async fn home(State(state): State<AppState>, Query(q): Query<TimelineQuery>) -> Response {
    let host = &state.config().server.host;
    let user = &state.config().server.user;

    let actor = match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(a)) if a.is_local => a,
        Ok(_) => {
            return error_with_body(
                StatusCode::SERVICE_UNAVAILABLE,
                "local actor not initialized; run `sakurasato init`",
            );
        }
        Err(err) => {
            error!(?err, "timeline/home: local actor lookup failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let limit = clamp_limit(q.limit);
    // before_ts_ms (epoch ミリ秒) を DateTime に。範囲外は無視 (= カーソル無し)。
    let until_ts: Option<DateTime<Utc>> = q
        .before_ts_ms
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single());

    // note window (時刻 bound) + renote window を別々に取り、`published_at` で
    // 1 本に merge する (#151 / MiAuth タイムラインと同じ方式)。
    let note_entries = match repo::note::list_home_timeline_window(
        state.pool(),
        actor.id,
        None,
        None,
        None,
        until_ts,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, "timeline/home: list_home_timeline_window failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let renote_rows = match repo::announce::list_home_renote_window(
        state.pool(),
        actor.id,
        None,
        until_ts,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, "timeline/home: list_home_renote_window failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    merge_and_respond(
        state.pool(),
        host,
        actor.id,
        note_entries,
        renote_rows,
        limit,
        "timeline/home",
    )
    .await
}

/// note window + renote window の解決・merge を 1 関数に集約したもの。
/// `home` (フォロー中スコープ) と `crate::local_api::user_list::list_timeline`
/// (リストメンバースコープ) は windows の取得元だけが異なり、以降の
/// 集計・merge・応答組み立ては完全に共通なので、本関数として切り出して両者
/// から呼ぶ。`log_prefix` はエラーログのタグ (呼び出し元を区別するため)。
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    reason = "timeline merge を 1 関数で組む / renoted・renoter は AP 用語"
)]
pub(crate) async fn merge_and_respond(
    pool: &sqlx::PgPool,
    host: &str,
    viewer_actor_id: i64,
    note_entries: Vec<TimelineEntry>,
    renote_rows: Vec<sakurasato_core::repo::announce::RenoteWindowRow>,
    limit: i64,
    log_prefix: &str,
) -> Response {
    // renote の元 note / renoter actor を一括解決。引けない場合は warn を残して
    // その renote を黙って落とす (= タイムライン本体は note で成立する)。
    let renoted_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoted_note_id).collect();
    let renoter_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoter_actor_id).collect();
    let renoted_entries = repo::note::list_timeline_entries_by_ids(pool, &renoted_ids)
        .await
        .unwrap_or_else(|err| {
            warn!(
                ?err,
                log_prefix, "renoted entries lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let renoter_actors = repo::actor::list_by_ids(pool, &renoter_ids)
        .await
        .unwrap_or_else(|err| {
            warn!(
                ?err,
                log_prefix, "renoter actors lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let entry_by_id: HashMap<i64, &TimelineEntry> =
        renoted_entries.iter().map(|e| (e.id, e)).collect();
    let actor_by_id: HashMap<i64, &ActorRow> = renoter_actors.iter().map(|a| (a.id, a)).collect();

    // note window + renote 元 note の全 id でリアクション / announce 集計。
    // 同じ note が note エントリと renote エントリの両方に出ることがあるので
    // `remove` ではなく `get(...).cloned()` で複数回引けるようにする。
    let mut all_note_ids: Vec<i64> = note_entries.iter().map(|e| e.id).collect();
    all_note_ids.extend(renoted_ids.iter().copied());
    // note エントリと renote 元が同一 note を指すと id が重複する。`counts_for_notes`
    // は `GROUP BY` + `= ANY()` 意味論で二重計上はされないが、配列を最小化して
    // 集計クエリを軽くし将来の脆さも断つため dedup する (#224 review)。
    all_note_ids.sort_unstable();
    all_note_ids.dedup();
    let mut by_note: HashMap<i64, Vec<ReactionSummaryDto>> = HashMap::new();
    match repo::reaction::counts_for_notes(pool, &all_note_ids).await {
        Ok(rows) => {
            for row in rows {
                by_note
                    .entry(row.note_id)
                    .or_default()
                    .push(row_to_dto(host, row));
            }
        }
        Err(err) => warn!(?err, log_prefix, "reaction counts_for_notes failed"),
    }
    let mut announce_by_note: HashMap<i64, sakurasato_core::repo::announce::AnnounceSummaryRow> =
        HashMap::new();
    match repo::announce::counts_for_notes(pool, &all_note_ids, viewer_actor_id).await {
        Ok(rows) => {
            for row in rows {
                announce_by_note.insert(row.note_id, row);
            }
        }
        Err(err) => warn!(?err, log_prefix, "announce counts_for_notes failed"),
    }

    // (sort_ts, TimelineNote) で merge。note は published_at、renote は renote 時刻。
    let mut items: Vec<(DateTime<Utc>, TimelineNote)> =
        Vec::with_capacity(note_entries.len() + renote_rows.len());
    for e in &note_entries {
        let reactions = by_note.get(&e.id).cloned().unwrap_or_default();
        let announce = announce_by_note.get(&e.id);
        items.push((
            e.published_at,
            TimelineNote::from_entry_with_aggregates(e, reactions, announce, host, None),
        ));
    }
    for r in &renote_rows {
        // 元 note / renoter が引けない renote はスキップ (FK 上は起きない)。
        let (Some(entry), Some(actor)) = (
            entry_by_id.get(&r.renoted_note_id),
            actor_by_id.get(&r.renoter_actor_id),
        ) else {
            continue;
        };
        let reactions = by_note.get(&entry.id).cloned().unwrap_or_default();
        let announce = announce_by_note.get(&entry.id);
        let meta = RenoteMeta {
            announce_id: r.announce_id,
            announce_ap_id: r.announce_ap_id.clone(),
            renoter_actor_id: actor.id,
            renoter_ap_id: actor.ap_id.clone(),
            renoter_preferred_username: actor.preferred_username.clone(),
            renoter_display_name: actor.display_name.clone(),
            renoter_icon_url: actor.icon_url.clone(),
            renoted_at: r.announce_published_at,
        };
        items.push((
            r.announce_published_at,
            TimelineNote::from_entry_with_aggregates(entry, reactions, announce, host, Some(meta)),
        ));
    }

    // 時刻降順。同時刻は note id 降順を tiebreak に。limit へ切る。
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

    let next_before_ts_ms = items.last().map(|(ts, _)| ts.timestamp_millis());
    let notes: Vec<TimelineNote> = items.into_iter().map(|(_, n)| n).collect();

    Json(TimelineResponse {
        notes,
        next_before_ts_ms,
    })
    .into_response()
}

pub(crate) fn clamp_limit(req: Option<i64>) -> i64 {
    let l = req.unwrap_or(LIMIT_DEFAULT);
    l.clamp(1, LIMIT_MAX)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamp_limit_uses_default_when_missing() {
        assert_eq!(clamp_limit(None), LIMIT_DEFAULT);
    }

    #[test]
    fn clamp_limit_caps_at_max() {
        assert_eq!(clamp_limit(Some(1000)), LIMIT_MAX);
    }

    #[test]
    fn clamp_limit_floors_at_one() {
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
    }

    #[test]
    fn parse_attachments_extracts_fields() {
        let raw = json!([
            {
                "type": "Document",
                "mediaType": "image/webp",
                "url": "https://e.example/m/1.webp",
                "name": "alt text",
                "width": 800,
                "height": 600,
            },
            {
                "type": "Image",
                "mediaType": "image/jpeg",
                "url": "https://e.example/m/2.jpg",
            },
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://e.example/m/1.webp");
        assert_eq!(out[0].media_type.as_deref(), Some("image/webp"));
        assert_eq!(out[0].alt.as_deref(), Some("alt text"));
        assert_eq!(out[0].width, Some(800));
        assert_eq!(out[0].height, Some(600));
        assert_eq!(out[1].url, "https://e.example/m/2.jpg");
        assert!(out[1].alt.is_none());
        assert!(out[1].width.is_none());
    }

    #[test]
    fn parse_attachments_skips_no_url() {
        // `url` 無しの entry は drop ── 表示できないため。
        let raw = json!([
            { "type": "Document", "mediaType": "image/webp" },
            { "type": "Document", "url": "https://e.example/ok.webp" },
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://e.example/ok.webp");
    }

    #[test]
    fn parse_attachments_empty_alt_dropped() {
        let raw = json!([{ "type": "Document", "url": "https://e.example/x.webp", "name": "" }]);
        let out = parse_attachments(&raw);
        assert!(out[0].alt.is_none());
    }

    #[test]
    fn parse_attachments_non_array_returns_empty() {
        assert!(parse_attachments(&JsonValue::Null).is_empty());
        assert!(parse_attachments(&json!({"key": "val"})).is_empty());
    }

    #[test]
    fn parse_attachments_rejects_non_http_schemes() {
        // round-2 review P3: `file://` / `javascript:` / `data:` などを
        // server 側で落とす (多層防御)。
        let raw = json!([
            { "url": "file:///etc/passwd" },
            { "url": "javascript:alert(1)" },
            { "url": "data:text/html,<script>" },
            { "url": "ftp://e.example/file" },
            { "url": "https://e.example/ok.webp" },
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://e.example/ok.webp");
    }

    #[test]
    fn parse_emojis_filters_type_emoji_only() {
        let raw = json!([
            {
                "type": "Emoji",
                "name": ":blob:",
                "icon": {"url": "https://local.test/media/emoji/local/blob.webp", "mediaType": "image/webp"}
            },
            {
                "type": "Mention",
                "name": "@alice@e.example",
                "href": "https://e.example/users/alice"
            },
            {
                "type": "Hashtag",
                "name": "#tag",
                "href": "https://e.example/tags/tag"
            },
            {
                "type": "Emoji",
                "name": ":remote@misskey.io:",
                "icon": {"url": "https://misskey.io/files/x.webp", "mediaType": "image/webp"}
            },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].shortcode, ":blob:");
        assert_eq!(out[0].is_local, Some(true));
        assert_eq!(out[1].shortcode, ":remote@misskey.io:");
        assert_eq!(out[1].is_local, Some(false));
    }

    #[test]
    fn parse_emojis_missing_icon_url_drops_image() {
        let raw = json!([
            { "type": "Emoji", "name": ":foo:" },
            { "type": "Emoji", "name": ":bar:", "icon": {} },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 2);
        assert!(out[0].image_url.is_none());
        assert!(out[1].image_url.is_none());
        // `image_url` が無いので `is_local` 判定不能 → `None`。
        assert!(out[0].is_local.is_none());
    }

    #[test]
    fn parse_emojis_empty_name_dropped() {
        let raw = json!([{ "type": "Emoji", "name": "" }]);
        let out = parse_emojis(&raw, "local.test");
        assert!(out.is_empty());
    }

    #[test]
    fn parse_emojis_non_array_returns_empty() {
        assert!(parse_emojis(&JsonValue::Null, "local.test").is_empty());
        assert!(parse_emojis(&json!({"x": 1}), "local.test").is_empty());
    }

    #[test]
    fn parse_emojis_drops_overly_long_shortcode() {
        // round-1 review ⚠️ 1: shortcode に長さ上限を設ける。
        let long_name = ":".to_string() + &"a".repeat(200) + ":";
        let raw = json!([{ "type": "Emoji", "name": long_name }]);
        let out = parse_emojis(&raw, "local.test");
        assert!(out.is_empty(), "200-char shortcode should be dropped");
    }

    #[test]
    fn parse_emojis_keeps_exactly_max_len_shortcode() {
        // 境界: 128 文字ちょうどは通す (上限は inclusive)。
        let name = ":".to_string() + &"a".repeat(126) + ":"; // 128 chars total
        assert_eq!(name.chars().count(), 128);
        let raw = json!([{ "type": "Emoji", "name": name.clone() }]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shortcode, name);
    }

    #[test]
    fn parse_attachments_caps_count() {
        // round-4 review F3: 1 Note あたり 32 件で truncate (悪意ある DoS 入力)。
        let mut arr = Vec::new();
        for i in 0..200 {
            arr.push(json!({ "url": format!("https://e.example/{i}.webp") }));
        }
        let raw = JsonValue::Array(arr);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), ATTACHMENTS_PER_NOTE_MAX);
    }

    #[test]
    fn parse_emojis_caps_count() {
        let mut arr = Vec::new();
        for i in 0..500 {
            arr.push(json!({ "type": "Emoji", "name": format!(":e{i}:") }));
        }
        let raw = JsonValue::Array(arr);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), EMOJIS_PER_NOTE_MAX);
    }

    #[test]
    fn parse_attachments_caps_alt_length() {
        // 1500 byte ちょうどは通し、1501 byte は drop。
        let alt_ok = "a".repeat(ATTACHMENT_ALT_MAX_BYTES);
        let alt_too_long = "a".repeat(ATTACHMENT_ALT_MAX_BYTES + 1);
        let raw = json!([
            {"url": "https://e.example/a.webp", "name": alt_ok.clone()},
            {"url": "https://e.example/b.webp", "name": alt_too_long},
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].alt.as_deref(), Some(alt_ok.as_str()));
        assert!(out[1].alt.is_none(), "overly long alt should be dropped");
    }

    #[test]
    fn parse_attachments_caps_url_length() {
        // round-6 review F1: URL に 2048 byte 上限を入れて DoS 入力を弾く。
        let long_url = "https://e.example/".to_string() + &"a".repeat(URL_MAX_BYTES);
        assert!(long_url.len() > URL_MAX_BYTES);
        let raw = json!([
            {"url": long_url},
            {"url": "https://e.example/ok.webp"},
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://e.example/ok.webp");
    }

    #[test]
    fn parse_emojis_caps_icon_url_length() {
        // round-6 review F2: emoji `icon.url` も 2048 byte 上限。
        let long_url = "https://e.example/".to_string() + &"a".repeat(URL_MAX_BYTES);
        let raw = json!([
            {
                "type": "Emoji",
                "name": ":big:",
                "icon": {"url": long_url}
            },
            {
                "type": "Emoji",
                "name": ":ok:",
                "icon": {"url": "https://e.example/ok.webp"}
            },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 2);
        assert!(out[0].image_url.is_none(), "long url should be dropped");
        assert!(out[1].image_url.is_some());
    }

    #[test]
    fn parse_attachments_caps_media_type_length() {
        let mt_too_long = "image/".to_string() + &"x".repeat(200);
        let raw = json!([
            {"url": "https://e.example/x.webp", "mediaType": mt_too_long}
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].media_type.is_none(),
            "overly long mediaType should be dropped"
        );
    }

    #[test]
    fn parse_emojis_rejects_non_http_icon_url() {
        // round-3 review Finding 2: `parse_attachments` と一貫して URL
        // スキームを `http(s)://` に絞る。落とした場合は `image_url` が
        // None だが、shortcode 自体は通過する (= 画像なしの emoji)。
        let raw = json!([
            {
                "type": "Emoji",
                "name": ":bad:",
                "icon": {"url": "file:///etc/passwd"}
            },
            {
                "type": "Emoji",
                "name": ":js:",
                "icon": {"url": "javascript:alert(1)"}
            },
            {
                "type": "Emoji",
                "name": ":ok:",
                "icon": {"url": "https://e.example/ok.webp"}
            },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 3, "shortcode 自体は drop しない");
        assert!(out[0].image_url.is_none());
        assert!(out[1].image_url.is_none());
        assert_eq!(
            out[2].image_url.as_deref(),
            Some("https://e.example/ok.webp")
        );
    }

    #[test]
    fn parse_emojis_shortcode_cap_counts_chars_not_bytes() {
        // round-2 review C2: 上限は **文字数** であって byte 数ではない。
        // CJK (1 文字 = 3 byte) でも 128 文字までは通す。byte 比較だと 43
        // 文字 (= 129 byte) で落ちるが、char 比較なら通る。
        let cjk_43 = "あ".repeat(43);
        assert!(cjk_43.len() > 128, "CJK 43 chars exceeds 128 bytes");
        assert!(cjk_43.chars().count() <= 128);
        let raw = json!([{ "type": "Emoji", "name": cjk_43.clone() }]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 1);
        // 逆に 129 文字 (= 387 byte) は char 上限超で drop。
        let cjk_129 = "あ".repeat(129);
        let raw2 = json!([{ "type": "Emoji", "name": cjk_129 }]);
        let out2 = parse_emojis(&raw2, "local.test");
        assert!(out2.is_empty());
    }

    #[test]
    fn parse_emojis_is_local_handles_port_in_local_host() {
        // round-1 review ⚠️ 2: `local_host` に port が混じっていても、
        // パースして `host_str()` 同士の比較で一致させる。
        let raw = json!([
            {
                "type": "Emoji",
                "name": ":foo:",
                "icon": {"url": "https://example.com/media/emoji/local/foo.webp"}
            }
        ]);
        // 通常パターン (port なし)。
        let no_port = parse_emojis(&raw, "example.com");
        assert_eq!(no_port[0].is_local, Some(true));
        // port が混じったパターン (= dev 環境)。同じ host_str に正規化されて
        // local 判定される。
        let with_port = parse_emojis(&raw, "example.com:8443");
        assert_eq!(with_port[0].is_local, Some(true));
    }

    // ── Issue #135: row_to_dto の image_key 解釈 ─────────────────────────

    fn reaction_row(image_key: Option<&str>, is_local: Option<bool>) -> ReactionSummaryRow {
        ReactionSummaryRow {
            note_id: 1,
            content: ":blob:".into(),
            count: 1,
            emoji_id: Some(1),
            image_key: image_key.map(str::to_string),
            media_type: Some("image/webp".into()),
            is_local,
            first_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn row_to_dto_local_key_expands_to_self_media_url() {
        // `emoji/local/...` は自鯖 `/media/...` URL になる (= 従来挙動)。
        let row = reaction_row(Some("emoji/local/sakura.webp"), Some(true));
        let dto = row_to_dto("local.test", row);
        assert_eq!(
            dto.emoji_image_url.as_deref(),
            Some("https://local.test/media/emoji/local/sakura.webp")
        );
        assert_eq!(dto.emoji_is_local, Some(true));
    }

    #[test]
    fn row_to_dto_remote_cached_key_expands_to_self_media_url() {
        // Issue #135: `emoji/remote/...` も自鯖 `/media/...` URL に展開する
        // (= TUI が相手サーバに直接 fetch しに行かなくて済む)。
        let row = reaction_row(Some("emoji/remote/misskey.io/blob.webp"), Some(false));
        let dto = row_to_dto("local.test", row);
        assert_eq!(
            dto.emoji_image_url.as_deref(),
            Some("https://local.test/media/emoji/remote/misskey.io/blob.webp")
        );
        assert_eq!(dto.emoji_is_local, Some(false));
    }

    #[test]
    fn row_to_dto_legacy_remote_url_passes_through() {
        // 旧 row (= remote URL を image_key に直接入れていた頃のデータ) は
        // 自鯖 prefix を被せず素通しする graceful migration。
        let row = reaction_row(Some("https://misskey.io/files/blob.webp"), Some(false));
        let dto = row_to_dto("local.test", row);
        assert_eq!(
            dto.emoji_image_url.as_deref(),
            Some("https://misskey.io/files/blob.webp")
        );
    }

    #[test]
    fn row_to_dto_null_image_key_yields_none() {
        // Issue #135: fetch 失敗で image_key=NULL の row はテキストフォールバック。
        let row = reaction_row(None, Some(false));
        let dto = row_to_dto("local.test", row);
        assert!(dto.emoji_image_url.is_none());
    }
}
