//! `MissUser` 変換層 (= M14 #158, 親 issue #150)。
//!
//! Sakurasato の [`ActorRow`] + 集計 count を Misskey 互換クライアントが
//! 期待する **`MissUser` (`UserLite` + `UserDetailed` 一部)** の JSON 形に変換する。
//!
//! ## AGPL discipline
//!
//! 本変換は [misskey-hub.net](https://misskey-hub.net/) の **公開 API 仕様** と
//! [api-doc.misskey.io](https://api-doc.misskey.io/) `OpenAPI` を一次資料とし、
//! Misskey の TypeScript handler は読まずに書く clean-room 実装 (=
//! [[agpl-discipline-miauth]] / [`crate::miauth`] module doc 参照)。フィールド
//! 名 / 型は **interface = 著作権対象外** (Oracle v Google) なので翻訳問題は
//! 起きない。
//!
//! ## #158 で返す `MissUser` の最小スコープ
//!
//! 親 issue #158 acceptance criteria より:
//!
//! ```text
//! id, name, username, host, avatarUrl, isLocked,
//! followersCount, followingCount, notesCount
//! ```
//!
//! ── Misskey の `UserLite` + `UserDetailed` の **必須フィールド** だけを抜き
//! 出した形。Milktea / `MissRirica` 等は他にも optional フィールド (= `avatarBlurhash`
//! / `bannerUrl` / `emojis` / `createdAt` 等) を読み得るが、Misskey 公式 JSON
//! schema 上いずれも **nullable** / **optional** なので、本 PR では必須フィー
//! ルドのみで「クライアントが panic しない最低限」を成立させる。Optional フィ
//! ールドは #159 (read endpoints) で `MissNote` / `MissEmoji` を追加する際に同
//! タイミングで揃える計画。
//!
//! ## ID 形式の差異
//!
//! Misskey の `id` は **string** (= `aidx` フォーマットの 16 文字 ULID-like)。
//! Sakurasato の `actor.id` は `BIGSERIAL` (= `i64`)。**クライアントは id を
//! opaque string として扱う** (= `AmazonOAuth` と同じ思想で「内部数値か文字列か
//! は問わず、サーバが返した値をそのまま echo」する) ので、本 PR では
//! `format!("{i64}")` で stringify した値を返す。Misskey 側と完全一致はしない
//! が **型一致** (= string) は維持する。
//!
//! parity test (`tests/federation/test_miauth_flow_parity.py`) は「`id` が
//! string であること + 同 client から見て後続 `/api/users/show?userId=<id>`
//! が同じ user を返すこと」までを検証し、**値の bit 同一性は要求しない**
//! (= 別 instance の actor を比較するので当然違う)。
//!
//! ## host の扱い
//!
//! Misskey は **local user に対しては `host: null`** を返す (= `UserLite` 仕様)。
//! Sakurasato でも `is_local == true` のとき `host: null` に倒す ── お一人様
//! サーバ前提で `/api/i` は常に local actor を返すため、実質常に null。
//! remote actor を返す経路 (= #159 で `users/show` を生やすとき) は host を
//! `Some(actor.host)` に倒す。

use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;

use sakurasato_core::model::ActorRow;
use sakurasato_core::repo::announce::AnnounceSummaryRow;
use sakurasato_core::repo::note::TimelineEntry;
use sakurasato_core::repo::reaction::ReactionSummaryRow;

/// `MissUser` (Misskey 互換) の最小サブセット。
///
/// `#[serde(rename_all = "camelCase")]` で `is_locked` → `isLocked` /
/// `followers_count` → `followersCount` のように JSON フィールド名を Misskey
/// 慣行に合わせる。`null` 表現は `Option<T>` (= Misskey 仕様の `nullable`)。
///
/// `Option<String>` フィールドは `serde` 既定で **null として出力** される
/// (= `skip_serializing_if = "Option::is_none"` は付けない)。Misskey の
/// `UserLite` spec で `name` / `host` / `avatarUrl` は **`null` 明示が必須** で、
/// `omitted` (= フィールドごと消える) は許容しないため。これは `serde_json`
/// のデフォルト挙動と一致する。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissUser {
    /// `actor.id` を stringify した値 ── Misskey は string、Sakurasato 内部は
    /// `i64` なので `format!("{}", id)` で変換する。後方互換性の観点では
    /// `i64` 直渡しも考えられるが、Misskey クライアントは string 前提で parse
    /// するので **型一致** のためにも string が正解。
    pub id: String,
    /// display name (= Sakurasato `actor.display_name`)。未設定なら `null`。
    pub name: Option<String>,
    /// `actor.preferred_username` (= local 部分のみ、`@` も `host` も含まない)。
    pub username: String,
    /// local user は `null`、remote user は `Some(host)`。詳細は module doc。
    pub host: Option<String>,
    /// `actor.icon_url`。未設定なら **identicon URL で fallback** ── M14 #174。
    ///
    /// misskey-dart の `UserLite.avatarUrl: Uri` は **non-null required** で、
    /// `null` を渡すと `_$UserLiteFromJson` が例外 → Aria iOS が
    /// timeline/profile 描画 ごと crash する。`actor.icon_url == None` のケース
    /// (= 直 init 後で avatar 未アップロード) で **identicon URL を合成** して
    /// 必ず string を返す。`/identicon/{id}` route 自体は未実装なので Aria は
    /// fetch で 404 を受けるが、default avatar に倒すだけで crash しない。
    pub avatar_url: String,
    /// `actor.manually_approves_followers` (= 鍵アカウント, #66 / M12)。
    /// Misskey の `isLocked` 慣行と完全に同義。
    pub is_locked: bool,
    /// `follow` テーブルの `state = 'accepted' AND followed = me` の件数。
    pub followers_count: i64,
    /// `follow` テーブルの `state = 'accepted' AND follower = me` の件数。
    pub following_count: i64,
    /// `note` テーブルの `is_local = TRUE` の件数 (お一人様サーバ前提で
    /// 「自分の投稿数」と同義)。
    pub notes_count: i64,
}

/// Sakurasato の [`ActorRow`] + 集計 count から `MissUser` を組み立てる。
///
/// 呼び出し側 (= [`crate::miauth::i`] handler) は `repo::actor::get_by_*` +
/// `repo::follow::count_*` + `repo::note::count_local` を直接叩いて値を集めて
/// から本関数に渡す。本関数はアロケーションだけで I/O を持たない (= unit test
/// で DB を立てずに変換ロジックだけ検証可能)。
pub fn from_actor_and_counts(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
) -> MissUser {
    MissUser {
        id: actor.id.to_string(),
        name: actor.display_name.clone(),
        username: actor.preferred_username.clone(),
        // local user は host を `null` で返す ── Misskey UserLite spec。
        host: if actor.is_local {
            None
        } else {
            Some(actor.host.clone())
        },
        // M14 #174: misskey-dart UserLite は avatarUrl: Uri (non-null required)。
        // icon_url が None でも crash しないよう identicon URL を合成する。
        avatar_url: actor
            .icon_url
            .clone()
            .unwrap_or_else(|| identicon_url_for(&actor.host, actor.id)),
        is_locked: actor.manually_approves_followers,
        followers_count,
        following_count,
        notes_count,
    }
}

/// `actor.icon_url == None` のケースで合成する identicon URL (= M14 #174)。
///
/// misskey-dart の `UserLite.avatarUrl` は **non-null Uri** で、`null` だと
/// `_$UserLiteFromJson` で例外 → Aria iOS が crash する。Sakurasato は
/// `/identicon/{id}` route を持たないが、URL 自体が string として valid なら
/// `Uri.parse` は通り (= Aria 側 fetch で 404 になっても default avatar
/// placeholder を出すだけ)、本関数の戻り値さえ valid URL なら crash 回避できる。
///
/// host は **actor.host** を使う (= remote actor も自分の host を持つので、
/// remote 経由 identicon URL を合成できる)。
pub(crate) fn identicon_url_for(host: &str, actor_id: i64) -> String {
    format!("https://{host}/identicon/{actor_id}")
}

// ── #159: MissNote / MissFile / MissEmoji (read endpoints) ──────────────────
//
// Misskey wire spec sources (公開 API 仕様、clean-room):
//
// - <https://misskey-hub.net/docs/api/>
// - <https://api-doc.misskey.io/> (= `/api-doc` の OpenAPI)
//
// observed wire shape は misskey-py (= YuzuRyo61/Misskey.py, MIT) で実 Misskey
// を叩いて確認する。`tests/federation/test_miauth_read_parity.py` が同形を
// observation-based に assert する。

/// Misskey の `visibility` enum と Sakurasato 内部 `Visibility` の対応。
///
/// | Sakurasato | Misskey   |
/// |------------|-----------|
/// | `public`   | `public`  |
/// | `unlisted` | `home`    |
/// | `followers`| `followers` |
/// | `direct`   | `specified` |
///
/// Misskey の `home` は「ホームタイムラインだけに流れる、不特定多数 timeline には
/// 出ない」= Mastodon の `unlisted` 相当。`specified` は「to で指定した宛先のみ」
/// = `direct` (DM) と同義。文字列が違うだけで意味は一致するので変換テーブルで
/// 受け持つ。
pub(crate) fn map_visibility(internal: &str) -> &'static str {
    match internal {
        "unlisted" => "home",
        "followers" => "followers",
        "direct" => "specified",
        // `public` も未知も `public` に倒す ── 不正な DB row が来ても
        // クライアント側 panic を避ける防御。
        _ => "public",
    }
}

/// Misskey 互換の `MissNote` (`Note` の wire shape)。
///
/// ## フィールドカバレッジ
///
/// 親 issue #159 acceptance: `id`, `createdAt`, `text`, `cw`, `userId`, `user`,
/// `replyId`, `reply`, `renoteId`, `renote`, `visibility`, `mentions`,
/// `fileIds`, `files`, `reactions`, `emojis`, `tags`, `uri`.
///
/// 本 PR (#159) は **`reply` / `renote` を常に `null`** で返す ── nested Note
/// を再帰展開するには bulk loader が再帰化して N+1 を引き戻すため。`replyId` /
/// `renoteId` で 2 段目クライアントから別 endpoint (`/api/notes/show`) を叩いて
/// 取得する規約とする (Misskey 公式クライアントも同じ動作)。
///
/// `reactions` は `BTreeMap<String, i64>` で **key 安定順** ── Misskey は
/// 「初回 reaction 時刻順」で並べるが、本 PR では key 辞書順で代替する
/// (= wire 上は `{}` で key 順序は仕様上未定義、テスト安定性のため固定)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissNote {
    pub id: String,
    pub created_at: String,
    pub text: Option<String>,
    pub cw: Option<String>,
    pub user_id: String,
    pub user: MissUser,
    pub reply_id: Option<String>,
    pub reply: Option<Box<MissNote>>,
    pub renote_id: Option<String>,
    pub renote: Option<Box<MissNote>>,
    pub visibility: String,
    pub mentions: Vec<String>,
    pub file_ids: Vec<String>,
    pub files: Vec<MissFile>,
    /// `{ ":shortcode:": count, "👍": count, ":shortcode@host:": count }` の object。
    /// JSON object なので `Vec<(K, V)>` は使えず `BTreeMap` で key 順を固定する。
    pub reactions: BTreeMap<String, i64>,
    /// `{ "shortcode": "url" }` ── `reactions` の `:shortcode:` key に対応する
    /// 画像 URL マップ。Unicode reaction には対応 entry を持たない。
    pub reaction_emojis: BTreeMap<String, String>,
    /// Misskey は `emojis` フィールドを返す (= 本文 `:foo:` の画像マップ)。
    /// 新仕様 (= `reactionEmojis` 統合) で deprecated 扱いだが、Milktea/
    /// `MissRirica` は `emojis` を読み続けているので両方出す。
    pub emojis: BTreeMap<String, String>,
    pub tags: Vec<String>,
    /// remote note は元 AP URI、local note は `Some(canonical_url)`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// local note の human-readable URL (= `https://<host>/notes/{id}`)。
    /// remote note では `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Misskey 互換の `MissFile` (= 添付メディア)。
///
/// AP の `Document` (= `note.attachments` JSONB 内の 1 要素) から組み立てる。
/// Sakurasato 内部の attachment は AP `Document` を素のまま JSON で保持しており、
/// `id` (= Misskey 側 file row の id) や `md5` を持たない。`id` は URL を hash
/// 化した安定 ID を発行することで wire 上の string 制約を満たす。
///
/// ## misskey-dart 互換 (= M14 #172 / Aria 実機検証で判明)
///
/// [shiosyakeyakini-info/misskey_dart](https://github.com/shiosyakeyakini-info/misskey_dart)
/// の `DriveFile` 定義は **`name: String` (non-null)** + **`properties:
/// DriveFileProperties` (non-null object)** が required。Dart の sound
/// null-safety で `name == null` を parse すると `_$DriveFileFromJson` が
/// 例外を投げ、Aria 等の client が timeline 描画ごと crash する。
///
/// 対応:
/// - `name` ── AP `Document.name` (= alt text) ではなく **URL の basename**
///   (= file 名相当) で組み立てる。原則 non-null。
/// - `comment` ── 引き続き AP `Document.name` (= alt text) を保持。
/// - `properties` ── 新規 [`MissFileProperties`] を required field として
///   emit。AP `Document` に width/height が無くても `{}` で valid。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissFile {
    /// `url` を SHA-256 して先頭 16 文字を取った値 (= 安定 string id)。
    /// `MissNote.fileIds` と一致する。
    pub id: String,
    /// AP `Document` には `published` が無いので、空文字または親 note の
    /// `createdAt` を使う ── 親 note 側で `MissNote::created_at` をコピー。
    pub created_at: String,
    /// **non-null** (= Misskey-dart の `DriveFile.name: String` 仕様準拠)。
    /// URL の basename を採用 ── AP `Document.name` は alt text で意味が違う
    /// ため [`Self::comment`] に置く。
    pub name: String,
    /// MIME type (= AP `mediaType`)。`application/octet-stream` を default に
    /// 倒す ── Misskey クライアントは type 無しを panic することがある。
    #[serde(rename = "type")]
    pub mime_type: String,
    /// MD5 は AP `Document` に無いので空文字。Misskey クライアントは
    /// 「存在チェックのみ」で使う傾向。null は許容されない (string 必須)。
    pub md5: String,
    /// AP `Document` には size が無いので 0。
    pub size: i64,
    pub url: String,
    pub thumbnail_url: Option<String>,
    /// AP `name` 相当 (= alt text)。Misskey-dart の `DriveFile.comment: String?`。
    pub comment: Option<String>,
    /// AP `sensitive` を継承。`note.sensitive` を全添付に伝搬する。
    pub is_sensitive: bool,
    /// Misskey-dart `DriveFile.properties` (= **required**) の表現。AP
    /// `Document` で width/height が指定されていれば伝搬、無ければ `null` で
    /// 埋める。全 field null でも有効 (= `{}` で OK)。
    pub properties: MissFileProperties,
}

/// Misskey-dart の `DriveFileProperties` 相当。全 field optional。
///
/// AP `Document` 内に `width` / `height` が指定されていれば
/// (`build_miss_file` 内で) 伝搬。`orientation` / `avg_color` は AP では
/// 表現が無いため常に `None`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissFileProperties {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub orientation: Option<i64>,
    pub avg_color: Option<String>,
}

/// `POST /api/emojis` レスポンスの 1 要素。
///
/// Misskey 公式 schema:
///
/// ```json
/// {
///   "aliases": ["..."],
///   "name": "shortcode",
///   "category": "..." | null,
///   "url": "https://...",
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct MissEmoji {
    pub aliases: Vec<String>,
    pub name: String,
    pub category: Option<String>,
    pub url: String,
}

/// Bulk-loaded per-note summary (reactions + announces) for N+1 avoidance.
///
/// `bulk_load_note_summaries` で `note_ids` を 1 回受け取り、`reactions` /
/// `announces` テーブルをそれぞれ 1 query で集約する。`note_id → summary` の
/// `HashMap` として返す ── handler はループ内で `HashMap::get` で参照するだけで、
/// timeline の N 件分の DB round-trip にならない。
pub(crate) struct NoteSummary {
    pub reactions: Vec<ReactionSummaryRow>,
    pub announce: Option<AnnounceSummaryRow>,
}

/// `note_ids` 全件分の reaction + announce 集計を **2 query** で取る。
///
/// - `reaction`: 1 query (= `repo::reaction::counts_for_notes`)
/// - `announce`: 1 query (= `repo::announce::counts_for_notes`)
///
/// 失敗時は空マップを返し、handler 側で「reactions: {}」「announce 無し」で
/// 描画継続する ── timeline 本体を 500 にしない。
pub(crate) async fn bulk_load_note_summaries(
    pool: &sqlx::PgPool,
    note_ids: &[i64],
    viewer_actor_id: i64,
) -> std::collections::HashMap<i64, NoteSummary> {
    let mut out: std::collections::HashMap<i64, NoteSummary> =
        std::collections::HashMap::with_capacity(note_ids.len());
    for &id in note_ids {
        out.insert(
            id,
            NoteSummary {
                reactions: Vec::new(),
                announce: None,
            },
        );
    }
    match sakurasato_core::repo::reaction::counts_for_notes(pool, note_ids).await {
        Ok(rows) => {
            for row in rows {
                let entry = out.entry(row.note_id).or_insert(NoteSummary {
                    reactions: Vec::new(),
                    announce: None,
                });
                entry.reactions.push(row);
            }
        }
        Err(err) => {
            tracing::warn!(?err, "miauth bulk reactions failed");
        }
    }
    match sakurasato_core::repo::announce::counts_for_notes(pool, note_ids, viewer_actor_id).await {
        Ok(rows) => {
            for row in rows {
                if let Some(entry) = out.get_mut(&row.note_id) {
                    entry.announce = Some(row);
                }
            }
        }
        Err(err) => {
            tracing::warn!(?err, "miauth bulk announces failed");
        }
    }
    out
}

/// `ReactionSummaryRow` 列を Misskey `reactions` object + `reactionEmojis` map
/// に変換する。
///
/// ## key 正規化
///
/// - Unicode reaction (`emoji_id IS NULL`, content = unicode 文字): そのまま
///   `"👍": count`
/// - local custom emoji (`is_local = true`): `:shortcode:` (content から `:` を
///   削除して shortcode 抜く → 再 wrap)
/// - remote custom emoji (`is_local = false`): `:shortcode@host:` 形に整える
///
/// **既存 `reaction.content` の保存形** は連合先によってばらつくが、おおむね:
/// - Misskey から来た local custom: `:foo@misskey.example:` 形
/// - Mastodon から来た: `:foo:` 形 + `emoji_id` で local resolve 済み
/// - Unicode: 単一の絵文字文字列 (`👍`)
///
/// 本関数は content を再正規化せず、key=content そのままを返す ── 連合相手が
/// 想定する key 形式と一致するため。`reactionEmojis` 用 URL は `image_key` から
/// 構築。
pub(crate) fn build_reactions(
    rows: &[ReactionSummaryRow],
    host: &str,
) -> (BTreeMap<String, i64>, BTreeMap<String, String>) {
    let mut reactions: BTreeMap<String, i64> = BTreeMap::new();
    let mut reaction_emojis: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        reactions.insert(row.content.clone(), row.count);
        let Some(image_key) = row.image_key.as_deref() else {
            continue;
        };
        // `:shortcode:` または `:shortcode@host:` の中身を取り出して
        // `reactionEmojis` map key に使う (= Misskey 慣行)。
        let key = trim_reaction_emoji_key(&row.content);
        let url = if row.is_local == Some(true) {
            format!("https://{host}/media/{image_key}")
        } else {
            // remote emoji は image_key 自体が absolute URL (= リモートサーバの
            // `Emoji.icon.url`)。
            image_key.to_string()
        };
        reaction_emojis.insert(key, url);
    }
    (reactions, reaction_emojis)
}

/// `:foo:` or `:foo@host:` の **外側 `:` を剥がす** ── `reactionEmojis` key 用。
/// Unicode (`:` を含まない) は touch せずそのまま返す。
fn trim_reaction_emoji_key(content: &str) -> String {
    let trimmed = content
        .strip_prefix(':')
        .and_then(|s| s.strip_suffix(':'))
        .unwrap_or(content);
    trimmed.to_string()
}

/// `note.tags` JSONB を走査し `type == "Emoji"` の `name` (= `:shortcode:`)
/// と icon URL の map を返す。Misskey 本文 `emojis` フィールド相当。
pub(crate) fn build_text_emojis(raw: &JsonValue, local_host: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let JsonValue::Array(arr) = raw else {
        return out;
    };
    for v in arr {
        if v.get("type").and_then(JsonValue::as_str) != Some("Emoji") {
            continue;
        }
        let Some(name) = v.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        let key = trim_reaction_emoji_key(name);
        let url = v
            .get("icon")
            .and_then(|i| i.get("url"))
            .and_then(JsonValue::as_str)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
        if let Some(url) = url {
            // local 判定が出来なくても `:foo:` 形は URL を素のまま入れて返す
            // (= local emoji なら自分の host 由来、remote なら相手 host 由来)。
            // ホスト比較は handler が必要に応じて行うが、本関数は MAP に絞る。
            let _ = local_host; // host 比較は将来用に引数だけ残す
            out.insert(key, url.to_string());
        }
    }
    out
}

/// `note.tags` JSONB から `type == "Hashtag"` の `name` を `#` 抜きで抽出する。
pub(crate) fn build_hashtags(raw: &JsonValue) -> Vec<String> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            if v.get("type").and_then(JsonValue::as_str) != Some("Hashtag") {
                return None;
            }
            let name = v.get("name").and_then(JsonValue::as_str)?;
            Some(name.trim_start_matches('#').to_string())
        })
        .collect()
}

/// `note.tags` JSONB から `type == "Mention"` の `href` を抽出する
/// (= AP URI のリスト)。
pub(crate) fn build_mentions(raw: &JsonValue) -> Vec<String> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            if v.get("type").and_then(JsonValue::as_str) != Some("Mention") {
                return None;
            }
            v.get("href")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// `note.attachments` JSONB 1 件 → `MissFile`。`url` が無い要素は `None`。
///
/// AP `Document.name` は alt text (= Mastodon 慣行) なので [`MissFile::comment`]
/// に置き、Misskey-dart 期待の non-null [`MissFile::name`] は **URL basename**
/// から組み立てる (= M14 #172、Aria 実機検証で判明)。
fn build_miss_file(raw: &JsonValue, created_at: &str, sensitive: bool) -> Option<MissFile> {
    let url = raw.get("url").and_then(JsonValue::as_str)?;
    let mime_type = raw
        .get("mediaType")
        .and_then(JsonValue::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    // AP `name` は alt text として `comment` に。
    let alt_text = raw
        .get("name")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // Misskey-dart `DriveFile.name` (= non-null) は URL basename を使う。
    let file_name = miss_file_name_from_url(url);
    let id = miss_file_id_from_url(url);
    // AP `Document` に width / height が乗っている場合 (= Mastodon / Misskey
    // 双方の連合は width/height を Document level に書く) は伝搬する。
    let width = raw.get("width").and_then(JsonValue::as_i64);
    let height = raw.get("height").and_then(JsonValue::as_i64);
    Some(MissFile {
        id,
        created_at: created_at.to_string(),
        name: file_name,
        mime_type,
        md5: String::new(),
        size: 0,
        url: url.to_string(),
        thumbnail_url: None,
        comment: alt_text,
        is_sensitive: sensitive,
        properties: MissFileProperties {
            width,
            height,
            orientation: None,
            avg_color: None,
        },
    })
}

/// URL の最後の path segment (= basename) を取り出す。
///
/// `https://host/media/abc.webp` → `abc.webp`
/// `https://host/media/abc.webp?v=1` → `abc.webp` (= query string は剥がす)
/// `https://host/` → `host` (= path が空なら host 名で fallback)
///
/// Misskey-dart の `DriveFile.name` 必須要件を満たすため。AP `Document.name`
/// (alt text) とは別物。
fn miss_file_name_from_url(url: &str) -> String {
    // query / fragment を剥がす。
    let path_only = url.split(['?', '#']).next().unwrap_or(url);
    let basename = path_only.rsplit('/').next().filter(|s| !s.is_empty());
    if let Some(name) = basename {
        return name.to_string();
    }
    // path が空 (= trailing `/`) の URL は host 部分を使う。それも無ければ URL
    // そのまま (= 異常 URL でも何か返して `null` を避ける、Aria crash 回避が
    // 第一優先)。
    url.split('/').nth(2).unwrap_or(url).to_string()
}

/// `url` の SHA-256 を取って先頭 16 hex 文字を返す ── 安定した opaque string id。
/// `sha2` クレートは workspace 依存にあり ── core 含め既に使われている。
fn miss_file_id_from_url(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(16);
    for b in &out[..8] {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `TimelineEntry` (= note row + actor 表示情報) と集計 summary を `MissNote`
/// に組み立てる。`MissNote.reply` / `renote` は本 PR では常に `None`。
pub(crate) fn timeline_entry_to_miss_note(
    entry: &TimelineEntry,
    summary: &NoteSummary,
    host: &str,
    self_actor_id: i64,
) -> MissNote {
    // **PR #165 round-2 fix**: `actor.is_local` を `entry.actor_ap_id` の host が
    // 自インスタンス (`host`) と一致するかで判定する。`entry_to_actor_lite` は
    // join に `actor.is_local` 列を持たないので false で構築するが、本関数で
    // 正しい値に上書きしてから `from_actor_and_counts` を呼ぶ。
    //
    // これを忘れると `MissUser.host` が local user でも `Some(host)` で emit
    // され、Misskey wire 仕様 (= local user は `host: null`) に違反する ──
    // Milktea / `MissRirica` 等は `host != null` のユーザを remote actor として
    // 扱うため、自分の投稿が「他インスタンスの user」として表示される。
    //
    // dev で `host = "example.com:8443"` のように port が混じってもパースで
    // `host_str` (= 純粋ホスト名) を取り出してから比較する ── `local_api/timeline.rs`
    // の `normalize_host_for_compare` と同じ流儀。
    let mut actor = entry_to_actor_lite(entry);
    actor.is_local = is_same_host(&entry.actor_ap_id, host);
    let user = from_actor_and_counts(&actor, 0, 0, 0);
    let _ = self_actor_id; // future use: user の count を埋める場合に

    let created_at = entry
        .published_at
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (reactions, reaction_emojis) = build_reactions(&summary.reactions, host);
    let emojis_in_text = build_text_emojis(&entry.tags.0, host);
    let mentions = build_mentions(&entry.tags.0);
    let tags = build_hashtags(&entry.tags.0);

    let mut files: Vec<MissFile> = Vec::new();
    let mut file_ids: Vec<String> = Vec::new();
    if let JsonValue::Array(arr) = &entry.attachments.0 {
        for v in arr {
            if let Some(f) = build_miss_file(v, &created_at, entry.sensitive) {
                file_ids.push(f.id.clone());
                files.push(f);
            }
        }
    }

    let uri = if entry.is_local {
        entry.url.clone()
    } else {
        Some(entry.ap_id.clone())
    };
    let url = if entry.is_local {
        entry.url.clone()
    } else {
        None
    };

    // AP `Note.content` (= HTML) を MFM 互換 plain text に倒す ── Misskey
    // クライアントは `text` を MFM として render するため、HTML タグが残ると
    // エスケープせず生で表示される (= #170 / Aria 実機検証で発覚)。
    // 空文字列 (= `<p></p>` 等の「HTML はあるが plain text は空」) は
    // **`None`** に倒す ── Misskey 仕様で本文無しの note は `text: null` (=
    // empty string ではなく省略) を期待。Aria など `text !== null` 分岐の
    // client が空テキストボックスを描画する事故を避ける。
    let stripped_text = crate::miauth::text::html_to_plain_text(&entry.content);
    let text = if stripped_text.is_empty() {
        None
    } else {
        Some(stripped_text)
    };

    MissNote {
        id: entry.id.to_string(),
        created_at,
        text,
        cw: entry.summary.clone(),
        user_id: entry.actor_id.to_string(),
        user,
        reply_id: entry.in_reply_to_note_id.map(|i| i.to_string()),
        reply: None,
        renote_id: None,
        renote: None,
        visibility: map_visibility(&entry.visibility).to_string(),
        mentions,
        file_ids,
        files,
        reactions,
        reaction_emojis,
        emojis: emojis_in_text,
        tags,
        uri,
        url,
    }
}

/// `actor_uri` のホストが `local_host` (= サーバ設定の `server.host`、port 付き
/// 可) と同一かを判定する。
///
/// 両辺を `url::Url::host_str()` 経由で正規化することで、`local_host` が
/// `"example.com:8443"` のような port 付き文字列でも `actor_uri =
/// "https://example.com/users/me"` と正しく一致させる。一致判定は ASCII-lowercase。
///
/// `actor_uri` がパースできない / host を持たない場合は `false` を返す
/// (= 安全側で remote 扱い) ── 不正データを local 扱いして
/// `MissUser.host: null` で emit してしまうリスクを避ける。
fn is_same_host(actor_uri: &str, local_host: &str) -> bool {
    let lhs = url::Url::parse(actor_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let rhs = url::Url::parse(&format!("https://{local_host}"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| local_host.to_ascii_lowercase());
    matches!(lhs, Some(h) if h == rhs)
}

/// `TimelineEntry` の join 部分 (= actor 表示情報のみ) を `ActorRow` 風に詰め直す。
/// `MissUser` の最小サブセット (id/name/username/host/avatarUrl/isLocked) しか
/// 使わないので、他フィールドは default で良い。
fn entry_to_actor_lite(entry: &TimelineEntry) -> ActorRow {
    use chrono::Utc;
    use sqlx::types::Json as SqlxJson;
    // host は entry の actor_ap_id から抜く ── timeline entry は preferred_username
    // と icon_url を直接持つが host を join していないので、ap_id を url::Url で
    // parse する。
    let host = url::Url::parse(&entry.actor_ap_id)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    ActorRow {
        id: entry.actor_id,
        ap_id: entry.actor_ap_id.clone(),
        preferred_username: entry.actor_preferred_username.clone(),
        host,
        display_name: entry.actor_display_name.clone(),
        summary: None,
        icon_url: entry.actor_icon_url.clone(),
        image_url: None,
        inbox_url: String::new(),
        shared_inbox_url: None,
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: String::new(),
        public_key_pem: String::new(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: SqlxJson(vec![]),
        moved_to_ap_id: None,
        // entry 自身は local/remote 判定を持たないので、actor_ap_id host を
        // 呼び出し側の local_host と比較する責務は handler 側にある。
        // ここでは false を default に倒す (= MissUser.host を `Some(host)` で
        // emit する) ── 呼び出し側で正しい値に上書きすること。
        is_local: false,
        actor_type: "Person".into(),
        manually_approves_followers: false,
        fetched_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// `ActorRow` を Misskey `UserDetailed` 相当に拡張変換する (= `/api/users/show`
/// 用)。`UserLite` 部分は `from_actor_and_counts` と同じ、追加で `createdAt`,
/// `description`, `bannerUrl`, `isBot`, `isCat` を載せる。
///
/// `isBot` / `isCat` は Misskey 独自 field ── `Person`/`Application`/`Service`
/// AP actor type の分岐で `isBot` を倒す。`isCat` は Sakurasato では常に `false`。
pub fn from_actor_detailed(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
) -> JsonValue {
    let lite = from_actor_and_counts(actor, followers_count, following_count, notes_count);
    let mut v = serde_json::to_value(lite).unwrap_or_else(|_| json!({}));
    if let JsonValue::Object(ref mut map) = v {
        map.insert(
            "createdAt".to_string(),
            JsonValue::String(
                actor
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        );
        map.insert(
            "description".to_string(),
            actor
                .summary
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
        map.insert(
            "bannerUrl".to_string(),
            actor
                .image_url
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
        let is_bot = matches!(actor.actor_type.as_str(), "Service" | "Application" | "Bot");
        map.insert("isBot".to_string(), JsonValue::Bool(is_bot));
        map.insert("isCat".to_string(), JsonValue::Bool(false));
    }
    v
}

/// `ActorRow` を Misskey `MeDetailed` 相当に拡張変換する (= `/api/i` 用、
/// M14 #170)。`UserDetailed` 部分は [`from_actor_detailed`] と同じ。
///
/// Me 専用フィールド (Misskey 仕様、お一人様前提のデフォルト):
///
/// - 認証 / 権限: `isAdmin` / `isModerator` / `isSilenced` / `isSuspended` ──
///   いずれも `false`。お一人様 server なので「自分はオーナー = 全部できる」
///   だが Misskey 側の `isAdmin`/`isModerator` は instance moderation role の
///   ことなので true を返しても client UI 上は変な挙動になりうる。`false` で
///   問題なし (= 全ての操作は通常 user として通る)。
/// - `roles: []` ── role 機能を持たないため空配列。
/// - `policies: {...}` ── `/api/meta.policies` と同じ shape。client は self の
///   permission チェックに使う。
/// - `emojis: {}` ── 自分の display name や description に絵文字を埋め込んだ
///   場合の shortcode → URL map。お一人様で empty map で十分。
/// - `onlineStatus: "unknown"` ── オンライン状態の追跡を実装しないため固定。
/// - `mfmEnabled: true` ── MFM 記法を `text` 上でレンダリングしてほしい。
/// - `isExplorable: true` ── public profile に出るかどうかの hint。お一人様で
///   特に隠す意味はない。
/// - `noindex: false`, `alwaysMarkNsfw: false` 等 ── プライバシー系の default。
/// - `avatarBlurhash` / `bannerBlurhash` / `bannerColor` ── null (= blurhash
///   生成は未実装、client 側は default placeholder を出す)。
///
/// 本関数は **`policies` の値を引数で受け取る** ── caller (= [`crate::miauth::i`])
/// が [`crate::miauth::meta::build_policies_value`] を介して `/api/meta` と
/// 完全に同じ JSON を渡す形にすることで、`/api/meta` と `/api/i` の policies
/// が乖離しない設計。
pub fn from_actor_me_detailed(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
    policies: JsonValue,
) -> JsonValue {
    let mut v = from_actor_detailed(actor, followers_count, following_count, notes_count);
    // `from_actor_detailed` は実質 `Object` を返すが、型レベルでは保証されて
    // いない。`Null` 等で来ると Me-only field 挿入が無音で消えるため、release
    // ビルドでも `error!` で気付ける形にする (= [PR #171 round-2 finding 2]
    // `debug_assert!` は release で no-op → 25+ MeDetailed field が silent 欠落)。
    if v.as_object_mut().is_none() {
        tracing::error!(
            actor_id = actor.id,
            "from_actor_detailed returned non-Object; MeDetailed fields will be dropped"
        );
        return v;
    }
    if let JsonValue::Object(ref mut map) = v {
        // Me-only flags (= 自分にしか出ないフィールド)。
        map.insert("isAdmin".to_string(), JsonValue::Bool(false));
        map.insert("isModerator".to_string(), JsonValue::Bool(false));
        map.insert("isSilenced".to_string(), JsonValue::Bool(false));
        map.insert("isSuspended".to_string(), JsonValue::Bool(false));
        map.insert("isExplorable".to_string(), JsonValue::Bool(true));
        map.insert("mfmEnabled".to_string(), JsonValue::Bool(true));
        map.insert("noindex".to_string(), JsonValue::Bool(false));
        map.insert("alwaysMarkNsfw".to_string(), JsonValue::Bool(false));
        map.insert("autoAcceptFollowed".to_string(), JsonValue::Bool(false));
        map.insert("publicReactions".to_string(), JsonValue::Bool(true));
        map.insert("hideOnlineStatus".to_string(), JsonValue::Bool(false));
        map.insert(
            "onlineStatus".to_string(),
            JsonValue::String("unknown".to_string()),
        );

        // 配列系 (= empty default)。
        map.insert("roles".to_string(), JsonValue::Array(vec![]));
        map.insert("badgeRoles".to_string(), JsonValue::Array(vec![]));
        map.insert("mutedWords".to_string(), JsonValue::Array(vec![]));
        map.insert("hardMutedWords".to_string(), JsonValue::Array(vec![]));
        map.insert("mutedInstances".to_string(), JsonValue::Array(vec![]));
        map.insert(
            "mutingNotificationTypes".to_string(),
            JsonValue::Array(vec![]),
        );
        map.insert("pinnedNoteIds".to_string(), JsonValue::Array(vec![]));
        map.insert("pinnedNotes".to_string(), JsonValue::Array(vec![]));
        map.insert("fields".to_string(), JsonValue::Array(vec![]));

        // object 系。
        map.insert("emojis".to_string(), serde_json::json!({}));
        map.insert("policies".to_string(), policies);

        // 画像系 (= blurhash 未生成、null で client 側 default に倒す)。
        map.insert("avatarBlurhash".to_string(), JsonValue::Null);
        map.insert("bannerBlurhash".to_string(), JsonValue::Null);
        map.insert("bannerColor".to_string(), JsonValue::Null);

        // Me 専用の付加情報。
        map.insert("email".to_string(), JsonValue::Null);
        map.insert("emailVerified".to_string(), JsonValue::Bool(false));
        map.insert("birthday".to_string(), JsonValue::Null);
        map.insert("location".to_string(), JsonValue::Null);
        map.insert("lang".to_string(), JsonValue::Null);
        map.insert("twoFactorEnabled".to_string(), JsonValue::Bool(false));
        map.insert("usePasswordLessLogin".to_string(), JsonValue::Bool(false));
        map.insert("securityKeys".to_string(), JsonValue::Bool(false));

        // M14 #174: misskey-dart の MeDetailed で **required bool** だが
        // Sakurasato が emit していなかった 13 件。すべて `false` で OK ──
        // お一人様 server で意味的に「自分の通知未読」「危険な投稿フラグ」など
        // 該当しないため。漏れていると `_$MeDetailedFromJson` で Dart sound
        // null-safety 例外 → Aria iOS が profile 描画ごと crash する。
        map.insert("injectFeaturedNote".to_string(), JsonValue::Bool(false));
        map.insert(
            "receiveAnnouncementEmail".to_string(),
            JsonValue::Bool(false),
        );
        map.insert("autoSensitive".to_string(), JsonValue::Bool(false));
        map.insert("carefulBot".to_string(), JsonValue::Bool(false));
        map.insert("noCrawle".to_string(), JsonValue::Bool(false));
        map.insert("isDeleted".to_string(), JsonValue::Bool(false));
        map.insert(
            "hasUnreadSpecifiedNotes".to_string(),
            JsonValue::Bool(false),
        );
        map.insert("hasUnreadMentions".to_string(), JsonValue::Bool(false));
        map.insert("hasUnreadAnnouncement".to_string(), JsonValue::Bool(false));
        map.insert("hasUnreadAntenna".to_string(), JsonValue::Bool(false));
        map.insert("hasUnreadChannel".to_string(), JsonValue::Bool(false));
        map.insert("hasUnreadNotification".to_string(), JsonValue::Bool(false));
        map.insert(
            "hasPendingReceivedFollowRequest".to_string(),
            JsonValue::Bool(false),
        );

        // M14 #174: required List/int だが Sakurasato が emit していなかった
        // 3 件。空配列 / 0 で必要十分。
        map.insert(
            "emailNotificationTypes".to_string(),
            JsonValue::Array(vec![]),
        );
        map.insert("achievements".to_string(), JsonValue::Array(vec![]));
        map.insert("loggedInDays".to_string(), JsonValue::Number(0.into()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sakurasato_core::model::ActorRow;
    use sqlx::types::Json as SqlxJson;

    fn fake_actor(is_local: bool, host: &str, locked: bool) -> ActorRow {
        ActorRow {
            id: 42,
            ap_id: format!("https://{host}/users/me"),
            preferred_username: "me".into(),
            host: host.into(),
            display_name: Some("Alice".into()),
            summary: None,
            icon_url: Some("https://cdn.test/avatar.webp".into()),
            image_url: None,
            inbox_url: format!("https://{host}/users/me/inbox"),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: "k".into(),
            public_key_pem: "p".into(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: SqlxJson(vec![]),
            moved_to_ap_id: None,
            is_local,
            actor_type: "Person".into(),
            manually_approves_followers: locked,
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// local actor の `host` は **null** で返る (Misskey `UserLite` 慣行)。
    #[test]
    fn local_actor_host_is_null() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 3, 5, 7);
        assert_eq!(miss.id, "42");
        assert_eq!(miss.username, "me");
        assert_eq!(miss.name.as_deref(), Some("Alice"));
        assert_eq!(miss.host, None, "local actor must serialize host as null");
        assert_eq!(miss.followers_count, 3);
        assert_eq!(miss.following_count, 5);
        assert_eq!(miss.notes_count, 7);
        assert!(!miss.is_locked);
    }

    /// remote actor の `host` は `Some(host)` で返る (将来 #159 で使う経路)。
    #[test]
    fn remote_actor_host_is_some() {
        let actor = fake_actor(false, "remote.test", false);
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        assert_eq!(miss.host.as_deref(), Some("remote.test"));
    }

    /// 鍵アカウント (`manually_approves_followers = true`) は `isLocked: true`。
    #[test]
    fn locked_actor_serializes_as_locked() {
        let actor = fake_actor(true, "sakurasato", true);
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        assert!(miss.is_locked);
    }

    /// camelCase + null 表現が Misskey wire spec と一致することを serde で確認。
    #[test]
    fn json_shape_matches_misskey_userlite_minimum() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 11, 12, 13);
        let json = serde_json::to_value(&miss).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["username"], "me");
        assert_eq!(json["name"], "Alice");
        assert!(
            json["host"].is_null(),
            "host must be JSON null, not omitted"
        );
        assert_eq!(json["avatarUrl"], "https://cdn.test/avatar.webp");
        assert_eq!(json["isLocked"], false);
        assert_eq!(json["followersCount"], 11);
        assert_eq!(json["followingCount"], 12);
        assert_eq!(json["notesCount"], 13);
    }

    /// `display_name` が未設定なら **null** で出力される (omitted ではなく)。
    /// Milktea は `name === null` でフォールバック表示する。
    ///
    /// `avatarUrl` は M14 #174 で **non-null required** に倒した ──
    /// `icon_url` が `None` でも identicon URL 合成で必ず string になる
    /// (= Aria の `_$UserLiteFromJson` クラッシュ回避)。
    #[test]
    fn optional_fields_serialize_as_null_when_missing() {
        let mut actor = fake_actor(true, "sakurasato", false);
        actor.display_name = None;
        actor.icon_url = None;
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        let json = serde_json::to_value(&miss).unwrap();
        assert!(json["name"].is_null());
        // **non-null**: icon_url が None でも identicon URL で fallback。
        assert!(json["avatarUrl"].is_string());
        let url = json["avatarUrl"].as_str().unwrap();
        assert!(url.starts_with("https://"));
        assert!(url.contains("/identicon/"));
    }

    // ── #159: visibility / file id / reactions / detailed user ────────────

    #[test]
    fn map_visibility_internal_to_misskey() {
        assert_eq!(map_visibility("public"), "public");
        assert_eq!(map_visibility("unlisted"), "home");
        assert_eq!(map_visibility("followers"), "followers");
        assert_eq!(map_visibility("direct"), "specified");
        // unknown は public に倒す。
        assert_eq!(map_visibility("garbage"), "public");
    }

    #[test]
    fn miss_file_id_is_stable_for_same_url() {
        let a = miss_file_id_from_url("https://e.example/a.webp");
        let b = miss_file_id_from_url("https://e.example/a.webp");
        let c = miss_file_id_from_url("https://e.example/b.webp");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16, "id must be 16 hex chars");
    }

    // ── M14 #172: misskey-dart `DriveFile` 互換 ───────────────────────────

    #[test]
    fn miss_file_name_uses_url_basename() {
        // 標準 URL は basename を返す。
        assert_eq!(
            miss_file_name_from_url("https://host/media/abc.webp"),
            "abc.webp"
        );
        // クエリ文字列付きは strip。
        assert_eq!(
            miss_file_name_from_url("https://host/media/photo.jpg?v=1"),
            "photo.jpg"
        );
        // fragment 付きも strip。
        assert_eq!(
            miss_file_name_from_url("https://host/media/photo.jpg#section"),
            "photo.jpg"
        );
        // trailing `/` (= path 空) は host 名を返す。
        assert_eq!(miss_file_name_from_url("https://host/"), "host");
    }

    #[test]
    fn build_miss_file_emits_non_null_name_from_url() {
        let raw = json!({
            "url": "https://example.test/media/abc123.webp",
            "mediaType": "image/webp",
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).expect("url is present");
        // `name` は string、URL basename。null ではない。
        assert_eq!(f.name, "abc123.webp");
    }

    #[test]
    fn build_miss_file_uses_ap_name_for_comment_not_for_name() {
        // AP `name` は alt text (= Mastodon 慣行) なので `comment` に置く。
        // Misskey-dart の non-null `DriveFile.name` 要件を満たすため、
        // file `name` は URL basename を使う。
        let raw = json!({
            "url": "https://example.test/media/abc.webp",
            "name": "Sakura petals in spring",
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).unwrap();
        assert_eq!(f.name, "abc.webp", "file name comes from URL basename");
        assert_eq!(
            f.comment.as_deref(),
            Some("Sakura petals in spring"),
            "AP name → comment (alt text)"
        );
    }

    #[test]
    fn build_miss_file_emits_properties_object() {
        // AP `Document` に width / height がある場合は properties に伝搬。
        let raw = json!({
            "url": "https://example.test/media/abc.webp",
            "width": 800,
            "height": 600,
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).unwrap();
        assert_eq!(f.properties.width, Some(800));
        assert_eq!(f.properties.height, Some(600));
        // AP には orientation / avg_color が無いので null。
        assert!(f.properties.orientation.is_none());
        assert!(f.properties.avg_color.is_none());
    }

    #[test]
    fn build_miss_file_emits_properties_object_when_no_width_height() {
        // width / height が無くても properties field 自体は必須 (= Misskey-dart の
        // `required DriveFileProperties` 要件)。全 null でも `{}` で valid。
        let raw = json!({
            "url": "https://example.test/media/audio.mp3",
            "mediaType": "audio/mpeg",
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).unwrap();
        // serialize して JSON shape を直接確認。
        let v = serde_json::to_value(&f).unwrap();
        assert!(v.get("properties").is_some(), "properties must be present");
        assert!(
            v["properties"].is_object(),
            "properties must be an object (= misskey-dart required field)"
        );
        // wire 上の `name` も string で null じゃない。
        assert!(
            v["name"].is_string(),
            "name must be a JSON string (= misskey-dart required field)"
        );
    }

    #[test]
    fn build_reactions_unicode_and_custom() {
        use chrono::Utc;
        let rows = vec![
            ReactionSummaryRow {
                note_id: 1,
                content: "👍".into(),
                count: 3,
                emoji_id: None,
                image_key: None,
                media_type: None,
                is_local: None,
                first_at: Utc::now(),
            },
            ReactionSummaryRow {
                note_id: 1,
                content: ":sakura:".into(),
                count: 1,
                emoji_id: Some(7),
                image_key: Some("emoji/local/sakura.webp".into()),
                media_type: Some("image/webp".into()),
                is_local: Some(true),
                first_at: Utc::now(),
            },
            ReactionSummaryRow {
                note_id: 1,
                content: ":blob@misskey.io:".into(),
                count: 2,
                emoji_id: Some(8),
                image_key: Some("https://misskey.io/files/blob.webp".into()),
                media_type: Some("image/webp".into()),
                is_local: Some(false),
                first_at: Utc::now(),
            },
        ];
        let (reactions, emojis) = build_reactions(&rows, "sakurasato.test");
        assert_eq!(reactions["👍"], 3);
        assert_eq!(reactions[":sakura:"], 1);
        assert_eq!(reactions[":blob@misskey.io:"], 2);
        // Unicode は reactionEmojis に乗らない。
        assert!(!emojis.contains_key("👍"));
        // local は /media/ URL に展開。
        assert_eq!(
            emojis["sakura"],
            "https://sakurasato.test/media/emoji/local/sakura.webp"
        );
        // remote は image_key の URL を素のまま。
        assert_eq!(
            emojis["blob@misskey.io"],
            "https://misskey.io/files/blob.webp"
        );
    }

    #[test]
    fn build_hashtags_extracts_names() {
        let tags = json!([
            {"type": "Hashtag", "name": "#sakurasato"},
            {"type": "Mention", "href": "https://x.test/u", "name": "@u@x.test"},
            {"type": "Hashtag", "name": "rust"},
        ]);
        let out = build_hashtags(&tags);
        assert_eq!(out, vec!["sakurasato", "rust"]);
    }

    #[test]
    fn build_mentions_returns_href_only() {
        let tags = json!([
            {"type": "Mention", "href": "https://x.test/users/alice", "name": "@alice@x.test"},
            {"type": "Emoji", "name": ":foo:"},
        ]);
        let out = build_mentions(&tags);
        assert_eq!(out, vec!["https://x.test/users/alice"]);
    }

    #[test]
    fn build_text_emojis_filters_emoji_type() {
        let tags = json!([
            {"type": "Emoji", "name": ":sakura:", "icon": {"url": "https://local.test/media/emoji/local/sakura.webp"}},
            {"type": "Mention", "href": "https://x.test/u"},
            {"type": "Emoji", "name": ":blob@misskey.io:", "icon": {"url": "https://misskey.io/files/blob.webp"}},
        ]);
        let out = build_text_emojis(&tags, "local.test");
        assert_eq!(
            out["sakura"],
            "https://local.test/media/emoji/local/sakura.webp"
        );
        assert_eq!(out["blob@misskey.io"], "https://misskey.io/files/blob.webp");
    }

    #[test]
    fn from_actor_detailed_adds_userdetailed_fields() {
        let mut actor = fake_actor(true, "sakurasato.test", false);
        actor.summary = Some("hello world".into());
        actor.image_url = Some("https://cdn.test/banner.webp".into());
        actor.actor_type = "Service".into();
        let v = from_actor_detailed(&actor, 1, 2, 3);
        assert_eq!(v["id"], "42");
        assert_eq!(v["description"], "hello world");
        assert_eq!(v["bannerUrl"], "https://cdn.test/banner.webp");
        assert_eq!(v["isBot"], true);
        assert_eq!(v["isCat"], false);
        // createdAt は ISO8601 で `Z` 終端。
        assert!(
            v["createdAt"].as_str().unwrap().ends_with('Z'),
            "createdAt must be UTC RFC3339 with Z suffix"
        );
    }

    // ── #165 round-2 fix: MissNote.user.host が local actor で null になる ──

    fn fake_timeline_entry(actor_ap_id: &str) -> TimelineEntry {
        use chrono::Utc;
        use sqlx::types::Json as SqlxJson;
        TimelineEntry {
            id: 100,
            ap_id: format!("{actor_ap_id}/note/100"),
            actor_id: 1,
            content: "hello".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: "public".into(),
            sensitive: false,
            to_recipients: SqlxJson(vec![]),
            cc_recipients: SqlxJson(vec![]),
            attachments: SqlxJson(json!([])),
            tags: SqlxJson(json!([])),
            is_local: true,
            url: Some(format!("{actor_ap_id}/note/100")),
            published_at: Utc::now(),
            edited_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            actor_ap_id: actor_ap_id.to_string(),
            actor_preferred_username: "alice".into(),
            actor_display_name: Some("Alice".into()),
            actor_icon_url: None,
        }
    }

    fn empty_summary() -> NoteSummary {
        NoteSummary {
            reactions: Vec::new(),
            announce: None,
        }
    }

    #[test]
    fn is_same_host_matches_plain_host() {
        assert!(is_same_host(
            "https://sakurasato.test/users/alice",
            "sakurasato.test"
        ));
        assert!(!is_same_host(
            "https://remote.test/users/alice",
            "sakurasato.test"
        ));
    }

    /// dev で `host = "example.com:8443"` (= port 付き) と
    /// `actor_uri = "https://example.com/users/me"` を一致させる。
    /// `local_api/timeline.rs::normalize_host_for_compare` と同じ流儀。
    #[test]
    fn is_same_host_ignores_port_in_local_host() {
        assert!(is_same_host(
            "https://example.com/users/me",
            "example.com:8443"
        ));
    }

    #[test]
    fn is_same_host_is_case_insensitive() {
        assert!(is_same_host(
            "https://Sakurasato.TEST/users/alice",
            "sakurasato.test"
        ));
    }

    /// 不正な `actor_uri` は false (= remote 扱い)。`host: null` が誤って
    /// 出ないことを保証する安全側 default。
    #[test]
    fn is_same_host_unparseable_actor_uri_is_remote() {
        assert!(!is_same_host("not a url", "sakurasato.test"));
        assert!(!is_same_host("https:///nopath", "sakurasato.test"));
    }

    /// **#165 round-2 bug fix の本丸**: local actor のノートに対する
    /// `MissNote.user.host` が **null** で emit される。
    #[test]
    fn timeline_entry_for_local_actor_emits_user_host_null() {
        let entry = fake_timeline_entry("https://sakurasato.test/users/alice");
        let summary = empty_summary();
        let note = timeline_entry_to_miss_note(&entry, &summary, "sakurasato.test", 1);
        let json = serde_json::to_value(&note).unwrap();
        assert!(
            json["user"]["host"].is_null(),
            "local actor note must serialize user.host as null, got {:?}",
            json["user"]["host"]
        );
        assert_eq!(json["user"]["username"], "alice");
    }

    /// remote actor のノートは `user.host` を `Some(remote_host)` で emit する。
    #[test]
    fn timeline_entry_for_remote_actor_emits_user_host_some() {
        let entry = fake_timeline_entry("https://misskey.io/users/bob");
        let summary = empty_summary();
        let note = timeline_entry_to_miss_note(&entry, &summary, "sakurasato.test", 1);
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(
            json["user"]["host"], "misskey.io",
            "remote actor note must serialize user.host with the remote host"
        );
    }

    /// dev で `local_host = "example.com:8443"` でも local user は
    /// `user.host: null` (= port 違いで remote 扱いされない)。
    #[test]
    fn timeline_entry_local_host_with_port_still_emits_null() {
        let entry = fake_timeline_entry("https://example.com/users/alice");
        let summary = empty_summary();
        let note = timeline_entry_to_miss_note(&entry, &summary, "example.com:8443", 1);
        let json = serde_json::to_value(&note).unwrap();
        assert!(
            json["user"]["host"].is_null(),
            "dev local_host with port must still emit user.host: null"
        );
    }
}
