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

/// 解決できない actor の fallback として使う空 `emojis` map。
/// (`HashMap::get` の `unwrap_or` 先 ── bulk 経路で毎回 `BTreeMap::new()` を
/// 作り直さないための shared reference。)
pub(crate) static EMPTY_EMOJIS: BTreeMap<String, String> = BTreeMap::new();

use sakurasato_core::model::{ActorRow, EmojiRow};
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
    /// ユーザーの `name` / `description` / `fields` に埋め込まれた `:shortcode:`
    /// の解決 map (= Misskey の `UserLite` / `UserDetailed` 共有フィールド)。
    ///
    /// Misskey wire では **常に存在する object** (空でも `{}`) で、key は
    /// コロン無し shortcode。クライアント (Aria 等) はこの map を使って
    /// displayName 内の `:foo:` を画像化する ── 空のまま名前に `:foo:` が
    /// 入ると生テキストで表示される (バグ 1 の根本原因)。
    pub emojis: BTreeMap<String, String>,
}

/// Sakurasato の [`ActorRow`] + 集計 count から `MissUser` を組み立てる。
///
/// 呼び出し側 (= [`crate::miauth::i`] handler) は `repo::actor::get_by_*` +
/// `repo::follow::count_*` + `repo::note::count_local` を直接叩いて値を集めて
/// から本関数に渡す。本関数はアロケーションだけで I/O を持たない (= unit test
/// で DB を立てずに変換ロジックだけ検証可能)。
///
/// `emojis` は呼び出し側が [`resolve_user_emojis`] で解決済みの shortcode →
/// URL map を渡す (= 本関数は純変換のまま、I/O を呼び出し側に残す)。
#[allow(clippy::too_many_arguments)]
pub fn from_actor_and_counts(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
    emojis: BTreeMap<String, String>,
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
        emojis,
    }
}

/// `ActorRow` の `display_name` / `summary` / `fields` (name + value) から
/// `:shortcode:` を抽出する (ASCII-lowercase + dedupe + 上限付き)。
///
/// [`crate::local_api::notes::parse_emoji_shortcodes`] を再利用する純関数で、
/// ユーザー `emojis` map (`resolve_user_emojis`) と actor `tag: [Emoji]`
/// (`crate::routes::actor::resolve_actor_emoji_tags`) が共有する ── 本文と
/// ユーザーで shortcode 抽出の文字種・大小正規化を drift させない。
pub(crate) fn collect_actor_emoji_shortcodes(actor: &ActorRow) -> Vec<String> {
    let mut shortcodes: Vec<String> = Vec::new();
    for text in [actor.display_name.as_deref(), actor.summary.as_deref()]
        .into_iter()
        .flatten()
        .chain(
            actor
                .fields
                .0
                .iter()
                .flat_map(|f| [f.name.as_str(), f.value.as_str()]),
        )
    {
        shortcodes.extend(crate::local_api::notes::parse_emoji_shortcodes(text));
    }
    shortcodes.sort_unstable();
    shortcodes.dedup();
    shortcodes
}

/// emoji 行の集合を `{shortcode: url}` map に畳む純関数。
///
/// - 同一 shortcode に local (`host IS NULL`) / learned-remote 両行がある場合
///   は **local 行優先** (remote 行は local 行で解決済みなら捨てる)。
/// - `image_key` が欠落している行は URL を組み立てられないのでスキップ
///   (= `list_by_shortcodes` の SQL 側でも `image_key IS NOT NULL` で弾いて
///   いるが、呼び出し経路によらず安全側の二重ガード)。
pub(crate) fn emoji_rows_to_url_map(rows: &[EmojiRow], host: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        if row.host.is_some() && out.contains_key(&row.shortcode) {
            continue;
        }
        let Some(image_key) = row.image_key.as_deref() else {
            continue;
        };
        out.insert(
            row.shortcode.clone(),
            crate::local_api::media::build_media_url(host, image_key),
        );
    }
    out
}

/// ユーザーの `display_name` / `summary` / `fields` (name + value) に埋め込ま
/// れた `:shortcode:` を DB の emoji 行に解決し、Misskey ユーザー `emojis`
/// フィールド用の `{shortcode: url}` map を返す。
///
/// - shortcode 抽出は [`collect_actor_emoji_shortcodes`] (= note 本文側と同じ
///   文字種・ASCII-lowercase 規約)。解決できない shortcode は黙って drop
///   (本文側と同じ fail-open)。
/// - URL は local / learned-remote とも自インスタンスの `build_media_url`
///   (= `/media/<image_key>`)。クライアントは `MiAuth` サーバ (= 自分) から
///   fetch するため remote オリジンを直に渡さない。
/// - 同一 shortcode に local / remote 両行がある場合は **local 行優先**
///   (= [`emoji_rows_to_url_map`])。
pub(crate) async fn resolve_user_emojis(
    pool: &sqlx::PgPool,
    host: &str,
    actor: &ActorRow,
) -> BTreeMap<String, String> {
    let shortcodes = collect_actor_emoji_shortcodes(actor);
    if shortcodes.is_empty() {
        return BTreeMap::new();
    }
    let rows = match sakurasato_core::repo::emoji::list_by_shortcodes(pool, &shortcodes).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, "resolve_user_emojis: list_by_shortcodes failed");
            return BTreeMap::new();
        }
    };
    emoji_rows_to_url_map(&rows, host)
}

/// 複数 actor の `emojis` map を一括解決して `actor_id → map` を返す。
///
/// bulk 経路 (timeline / streaming / lists / notifications の entry 列挙) で
/// entry ごとに [`resolve_user_emojis`] を呼ばない (N+1 抑止) ための共通
/// ヘルパー。actor の batch fetch + 各 actor の `emojis` 解決を 1 回に畳む。
/// DB エラー時は fail-open (該当 actor は空 map)。
pub(crate) async fn resolve_user_emojis_by_ids(
    pool: &sqlx::PgPool,
    host: &str,
    actor_ids: &[i64],
) -> std::collections::HashMap<i64, BTreeMap<String, String>> {
    let mut ids: Vec<i64> = actor_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut out = std::collections::HashMap::with_capacity(ids.len());
    let Ok(actors) = sakurasato_core::repo::actor::list_by_ids(pool, &ids).await else {
        return out;
    };
    for a in &actors {
        out.insert(a.id, resolve_user_emojis(pool, host, a).await);
    }
    out
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
    /// Misskey `Note.myReaction` ── 認証 viewer 自身がこの note に付けた reaction の
    /// content (`:foo:` / `👍`)、無ければ `null`。`reactions` map の key と一致する。
    /// これが無いと Aria 等は viewer 自身の reaction を別物として重複表示する。
    /// Misskey wire は常にこの field を出す (null 含む) ので `skip_serializing_if`
    /// は付けない。
    pub my_reaction: Option<String>,
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
    /// viewer 自身がこの note に付けた reaction content (= `Note.myReaction`)。
    /// 反応していなければ `None`。
    pub my_reaction: Option<String>,
}

/// `note_ids` 全件分の reaction + announce + `myReaction` 集計を **3 query** で取る。
///
/// - `reaction`: 1 query (= `repo::reaction::counts_for_notes`)
/// - `announce`: 1 query (= `repo::announce::counts_for_notes`)
/// - `myReaction`: 1 query (= `repo::reaction::my_reactions_for_notes`、viewer-scope)
///
/// 失敗時は空マップを返し、handler 側で「reactions: {}」「announce 無し」
/// 「`myReaction`: null」で描画継続する ── timeline 本体を 500 にしない。
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
                my_reaction: None,
            },
        );
    }
    match sakurasato_core::repo::reaction::counts_for_notes(pool, note_ids).await {
        Ok(rows) => {
            for row in rows {
                let entry = out.entry(row.note_id).or_insert(NoteSummary {
                    reactions: Vec::new(),
                    announce: None,
                    my_reaction: None,
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
    match sakurasato_core::repo::reaction::my_reactions_for_notes(pool, note_ids, viewer_actor_id)
        .await
    {
        Ok(rows) => {
            for row in rows {
                if let Some(entry) = out.get_mut(&row.note_id) {
                    entry.my_reaction = Some(row.content);
                }
            }
        }
        Err(err) => {
            tracing::warn!(?err, "miauth bulk my_reactions failed");
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
        // Issue #135: local / remote 共に versitygw キー形式 (`emoji/...`) を
        // 自鯖 `/media/` URL に展開。旧 row (= remote URL を `image_key` に
        // 直接入れていた頃のデータ) は `http(s)://` 形式なので素通し ──
        // 再 upsert で `emoji/remote/...` に書き換わるまでの graceful。
        let url = if is_absolute_url(image_key) {
            image_key.to_string()
        } else {
            format!("https://{host}/media/{image_key}")
        };
        reaction_emojis.insert(key, url);
    }
    (reactions, reaction_emojis)
}

/// `image_key` が絶対 URL (= 旧 row のリモート pass-through データ) かを判定する。
/// 真なら自鯖 `/media/` URL の prefix を被せず素通しする。Issue #135 で nullable 化 +
/// 自鯖キャッシュに切り替えた経路の **graceful migration** 用。
fn is_absolute_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
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
    // `mime_type` は下の struct field に move するので、画像判定は先に取る。
    let is_image = mime_type.starts_with("image/");
    Some(MissFile {
        id,
        created_at: created_at.to_string(),
        name: file_name,
        mime_type,
        md5: String::new(),
        size: 0,
        url: url.to_string(),
        // Aria 等の Misskey クライアントはタイムラインのインラインサムネイルに
        // `thumbnailUrl` を使う。null だとサムネイルが出ず、タップ時の `url`
        // (= フル画像) しか開けない (Aria 実機検証で判明)。Sakurasato は添付を
        // `preview` variant (≤1280px webp) 1 枚で保存し別サムネイルを持たないので、
        // **画像なら `url` をそのまま thumbnail にも使う** (= 既に十分小さい)。
        // 動画/音声等の非画像は画像サムネイルが無いので `null` のまま。
        thumbnail_url: is_image.then(|| url.to_string()),
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

/// `MediaRow` (= 自鯖が R2 に持つ media) を Misskey `DriveFile` (`MissFile`) に
/// 変換する。MiAuth `drive/files/*` で使う。
///
/// **`id` は media 行 id** (= `notes/create` の `fileIds` が parse する数値) に
/// 揃える ── [`build_miss_file`] の url-hash id とは別物 (あちらは `MissNote.files`
/// の表示用)。media は media-proxy で webp 化済みなので常に画像 = `thumbnailUrl`
/// も同一 URL。`isSensitive` は media 行に持たないので `false` 固定 (Sakurasato は
/// 添付の sensitive を note 側で持つ)。
pub(crate) fn media_row_to_miss_file(
    row: &sakurasato_core::model::MediaRow,
    host: &str,
) -> MissFile {
    let url = format!("https://{host}/media/{}", row.storage_key);
    let name = miss_file_name_from_url(&url);
    MissFile {
        id: row.id.to_string(),
        created_at: row
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        name,
        mime_type: row.media_type.clone(),
        md5: String::new(),
        size: row.byte_size,
        url: url.clone(),
        thumbnail_url: Some(url),
        comment: row.alt_text.clone(),
        is_sensitive: false,
        properties: MissFileProperties {
            width: Some(i64::from(row.width)),
            height: Some(i64::from(row.height)),
            orientation: None,
            avg_color: None,
        },
    }
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
///
/// `user_emojis` は **entry の actor の** `emojis` map。本関数は sync で
/// I/O を持たないため、呼び出し側 (bulk 経路は [`resolve_user_emojis_by_ids`]
/// で actor ごとに 1 回解決) が渡す。
pub(crate) fn timeline_entry_to_miss_note(
    entry: &TimelineEntry,
    summary: &NoteSummary,
    host: &str,
    user_emojis: &BTreeMap<String, String>,
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
    let user = from_actor_and_counts(&actor, 0, 0, 0, user_emojis.clone());

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
        my_reaction: summary.my_reaction.clone(),
        emojis: emojis_in_text,
        tags,
        uri,
        url,
    }
}

/// 自分の renote (= Announce / boost) を Misskey の renote `MissNote` 形に合成する。
///
/// Sakurasato は renote を `announce` テーブルで持ち **独立した note 行を発行しない**
/// ため、`notes/create { renoteId }` のレスポンス (= `createdNote`) は
/// announce の id / `ap_id` + 元 note (`renoted`) + renoter から組み立てる。Misskey の
/// renote 形に揃えて `text` は `null`、元 note を `renote` に nest し、`renoteId`
/// で参照する。`reply` / 添付 / reaction は renote 自体には付かないので空。
#[allow(clippy::similar_names)] // renoter (= 行為者) / renoted (= 対象) は AP 用語
pub(crate) fn build_renote_miss_note(
    announce_id: i64,
    announce_ap_id: &str,
    created_at: &str,
    renoter: MissUser,
    renoter_actor_id: i64,
    renoted: MissNote,
) -> MissNote {
    MissNote {
        // **id 名前空間**: announce.id と note.id は別連番なので、renote の MissNote
        // id を素の announce_id にすると home timeline で note と衝突する
        // (= 同じ数値 id の note/renote が混ざるとクライアントが取り違える)。
        // `rn:` prefix で名前空間を分ける。`notes/show` も同 prefix を解す。
        // `renote_id` (= nest した元 note の id) は素の note id のまま。
        id: format!("rn:{announce_id}"),
        created_at: created_at.to_string(),
        text: None,
        cw: None,
        user_id: renoter_actor_id.to_string(),
        user: renoter,
        reply_id: None,
        reply: None,
        renote_id: Some(renoted.id.clone()),
        renote: Some(Box::new(renoted)),
        // renote 可能なのは public / unlisted のみ (= local_api renote が enforce)。
        // renote 自体の visibility は public で返す。
        visibility: "public".to_string(),
        mentions: Vec::new(),
        file_ids: Vec::new(),
        files: Vec::new(),
        reactions: BTreeMap::new(),
        reaction_emojis: BTreeMap::new(),
        // renote wrapper 自体は reaction を持たない (= nested `renoted` が持つ)。
        my_reaction: None,
        emojis: BTreeMap::new(),
        tags: Vec::new(),
        uri: if announce_ap_id.is_empty() {
            None
        } else {
            Some(announce_ap_id.to_string())
        },
        url: None,
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
        birthday: None,
        location: None,
        lang: None,
        followed_message: None,
        fields: SqlxJson(vec![]),
        followers_count: 0,
        following_count: 0,
        notes_count: 0,
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
///
/// `relationship` は viewer (= ローカル actor) から見た target との follow 関係
/// ([`crate::follow::compute_follow_relationship`] の結果)。`UserDetailedNotMe`
/// の required bool `isFollowing` / `isFollowed` / `hasPendingFollowRequestFromYou`
/// / `hasPendingFollowRequestToYou` にそのまま載せる。自分自身 (`/api/i` 経由) は
/// [`crate::follow::FollowRelationship::neutral`] を渡せば良い。
#[allow(clippy::too_many_arguments)]
pub fn from_actor_detailed(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
    relationship: crate::follow::FollowRelationship,
    emojis: BTreeMap<String, String>,
) -> JsonValue {
    let lite = from_actor_and_counts(actor, followers_count, following_count, notes_count, emojis);
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
        // `description` (= bio) は Misskey クライアントが **MFM (plain text)**
        // として render する。AP actor の `summary` は **remote のとき HTML**
        // (Mastodon / Misskey が `<p>` / `<a>` 等で配送) なので、無変換で載せると
        // Aria 等で生タグが見える (= #271、Note 本文に対する #170 と同型)。
        // [`crate::miauth::text::html_to_plain_text`] で plain 化する。
        //
        // 一方 **local** actor の `summary` は TUI 入力をそのまま保存し AP actor
        // JSON にも raw emit する plain text なので、変換すると `price < 100` の
        // ような `<` が tag として strip され壊れる。`actor.is_local` で分岐し、
        // remote のときだけ変換する。
        let description = match actor.summary.as_deref() {
            Some(s) if !actor.is_local => {
                JsonValue::String(crate::miauth::text::html_to_plain_text(s))
            }
            Some(s) => JsonValue::String(s.to_string()),
            None => JsonValue::Null,
        };
        map.insert("description".to_string(), description);
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

        // `birthday` / `location` / `lang` / `fields` は misskey-dart の
        // `UserDetailed` 共有 interface (= Me / NotMe 両方) が持つフィールド
        // なので、Me 専用ではなくここ (基底) に置く。MiAuth `i/update`
        // (`crate::miauth::i::update`) が書き込む実データをそのまま返す。
        // remote actor はこれらの列を書き込む経路が無いため常に空/null。
        map.insert(
            "birthday".to_string(),
            actor
                .birthday
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
        map.insert(
            "location".to_string(),
            actor
                .location
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
        map.insert(
            "lang".to_string(),
            actor
                .lang
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
        map.insert(
            "fields".to_string(),
            JsonValue::Array(
                actor
                    .fields
                    .0
                    .iter()
                    .map(|f| json!({ "name": f.name, "value": f.value }))
                    .collect(),
            ),
        );

        // misskey-dart の `UserDetailedNotMe` で **required bool** (= 非 null,
        // default 無し) なのに `/api/users/show` / `/api/users/{following,
        // followers}` で emit していなかった 3 件。漏れていると Aria の
        // `_$UserDetailedNotMeFromJson` が Dart sound null-safety で例外を投げ、
        // `MisskeyUsers.show` → `UserDetailed.fromJson` ごと crash する (= #174
        // と同型のバグだが MeDetailed ではなく UserDetailed 側)。`from_actor_me_detailed`
        // (= `/api/i`) は元から emit していたため `/api/i` だけ通っていた。
        // いずれも UserDetailed 共有 field (= Me / NotMe 両方に存在) なので
        // 基底のここに置く。`isSilenced`/`isSuspended` は per-actor moderation
        // 状態を Sakurasato が持たないため常に false、`publicReactions` は
        // リアクションが公開である我々の前提で true。
        map.insert("isSilenced".to_string(), JsonValue::Bool(false));
        map.insert("isSuspended".to_string(), JsonValue::Bool(false));
        map.insert("publicReactions".to_string(), JsonValue::Bool(true));

        // **#348 系**: misskey-dart の `UserDetailedNotMe` で **required bool**
        // なのに `/api/users/show` で emit していなかった follow relationship
        // 4 件。漏れていると Aria が「フォローされています」等を出せず、`_$
        // UserDetailedNotMeFromJson` の required read で crash し得る (#174 と
        // 同型)。`isFollowing` (= viewer → target accepted) / `isFollowed`
        // (= target → viewer accepted) は [`crate::follow::compute_follow_relationship`]
        // の結果をそのまま載せ、`hasPendingFollowRequest*` は pending 状態を
        // 反映する。自分自身への `/api/i` では呼び出し側が `neutral()` を渡す
        // ため全て false になる (= Misskey も自己参照で false / 意味論同等)。
        map.insert(
            "isFollowing".to_string(),
            JsonValue::Bool(relationship.following),
        );
        map.insert(
            "isFollowed".to_string(),
            JsonValue::Bool(relationship.followed_by),
        );
        map.insert(
            "hasPendingFollowRequestFromYou".to_string(),
            JsonValue::Bool(relationship.has_pending_follow_request_from_you),
        );
        map.insert(
            "hasPendingFollowRequestToYou".to_string(),
            JsonValue::Bool(relationship.has_pending_follow_request_to_you),
        );

        // **misskey_dart (shiosyakeyakini-info, MIT) `UserDetailedNotMeWithRelations`**
        // は `UserDetailed.fromJson` が `isFollowing` key の有無だけで分岐するため、
        // 上の `isFollowing` を emit した時点で必ずこの型で parse される。その
        // generated `_$UserDetailedNotMeWithRelationsFromJson` は `isBlocking` /
        // `isBlocked` / `isMuted` / `isRenoteMuted` も **required bool** として
        // 読み、欠けると `type 'Null' is not a subtype of type 'bool'` で crash
        // する (= #174 と同型の required bool 漏れの WithRelations 版。Aria の
        // `UserNotifier` で実際に発生した)。Sakurasato はブロック / ミュート
        // 機能を持たないため全て固定 `false`。`notify` / `withReplies` は
        // nullable だが、wire parity のため Misskey の既定値 (normal / true) を
        // 載せておく。
        map.insert("isBlocking".to_string(), JsonValue::Bool(false));
        map.insert("isBlocked".to_string(), JsonValue::Bool(false));
        map.insert("isMuted".to_string(), JsonValue::Bool(false));
        map.insert("isRenoteMuted".to_string(), JsonValue::Bool(false));
        map.insert("notify".to_string(), JsonValue::String("normal".into()));
        map.insert("withReplies".to_string(), JsonValue::Bool(true));

        // **必須ではなく key の存在自体が意味を持つフィールド**: misskey_dart の
        // `User.fromJson` (= `MisskeyUsers.search` / `searchByUsernameAndHost` が
        // 使う polymorphic factory) は `json.containsKey("url")` の有無だけで
        // `UserLite.fromJson` (無し) / `UserDetailed.fromJson` (有り) を出し分ける。
        // 本関数はこれまで `url` キー自体を emit していなかったため、値の
        // 妥当性とは無関係に **常に `UserLite` として parse され**、Aria の
        // `SearchUsersNotifier._fetchUsers` が `response.whereType<UserDetailed>()`
        // で全件除外 → 200 + 非空 JSON なのに検索結果が常に空、という症状に
        // なっていた (2026-07-22 調査)。Sakurasato は AP actor の `url` (=
        // ActivityPub `id` とは別の「人間可読ページ」property) を保存して
        // いないため値は常に `null` にする ── **null でも key が存在すれば
        // `containsKey` は true** なので、これだけで UserDetailed 経路に乗る。
        map.insert("url".to_string(), JsonValue::Null);
    }
    v
}

/// `ActorRow` を Misskey `MeDetailed` 相当に拡張変換する (= `/api/i` 用、
/// M14 #170)。`UserDetailed` 部分は [`from_actor_detailed`] と同じ。
///
/// Me 専用フィールド (Misskey 仕様、お一人様前提のデフォルト):
///
/// - 認証 / 権限: `isAdmin` / `isModerator` ── いずれも `false`。お一人様
///   server なので「自分はオーナー = 全部できる」だが Misskey 側の
///   `isAdmin`/`isModerator` は instance moderation role のことなので true を
///   返しても client UI 上は変な挙動になりうる。`false` で問題なし (= 全ての
///   操作は通常 user として通る)。`isSilenced` / `isSuspended` /
///   `publicReactions` は Me 専用ではなく `UserDetailed` 共有 field なので
///   [`from_actor_detailed`] で挿入済み (= ここでは触らない)。
/// - `roles: []` ── role 機能を持たないため空配列。
/// - `policies: {...}` ── `/api/meta.policies` と同じ shape。client は self の
///   permission チェックに使う。
/// - `emojis` ── 自分の display name や description に絵文字を埋め込んだ場合の
///   shortcode → URL map。`from_actor_detailed` 経由で呼び出し側が解決済みの
///   map を渡す (= `resolve_user_emojis`)。従来は `{}` 固定で、名前に `:foo:`
///   を入れると Aria 等で生テキスト表示になっていた (バグ 1)。
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
#[allow(clippy::too_many_arguments)]
pub fn from_actor_me_detailed(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
    policies: JsonValue,
    emojis: BTreeMap<String, String>,
) -> JsonValue {
    // 自分自身 (Me) に対しては follow relationship は常に中立 ── `/api/i` の
    // `UserDetailedNotMe` 部分も required bool が揃っている限り client は
    // 描画に困らない。
    let mut v = from_actor_detailed(
        actor,
        followers_count,
        following_count,
        notes_count,
        crate::follow::FollowRelationship::neutral(),
        emojis,
    );
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
        // `isSilenced` / `isSuspended` / `publicReactions` は UserDetailed 共有
        // field なので [`from_actor_detailed`] (基底) で挿入済み ── ここで重複
        // させない。
        map.insert("isAdmin".to_string(), JsonValue::Bool(false));
        map.insert("isModerator".to_string(), JsonValue::Bool(false));
        map.insert("isExplorable".to_string(), JsonValue::Bool(true));
        map.insert("mfmEnabled".to_string(), JsonValue::Bool(true));
        map.insert("noindex".to_string(), JsonValue::Bool(false));
        map.insert("alwaysMarkNsfw".to_string(), JsonValue::Bool(false));
        map.insert("autoAcceptFollowed".to_string(), JsonValue::Bool(false));
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
        // `fields` は `UserDetailed` 共有フィールドなので [`from_actor_detailed`]
        // (基底) で実データを挿入済み ── ここで空配列に上書きしない。
        // `emojis` も UserDetailed 共有フィールドで、基底が呼び出し側から
        // 渡された実 map を emit 済み ── ここで `{}` に上書きしない (バグ 1)。

        // object 系。
        map.insert("policies".to_string(), policies);

        // 画像系 (= blurhash 未生成、null で client 側 default に倒す)。
        map.insert("avatarBlurhash".to_string(), JsonValue::Null);
        map.insert("bannerBlurhash".to_string(), JsonValue::Null);
        map.insert("bannerColor".to_string(), JsonValue::Null);

        // Me 専用の付加情報。
        map.insert("email".to_string(), JsonValue::Null);
        map.insert("emailVerified".to_string(), JsonValue::Bool(false));
        // `birthday` / `location` / `lang` は `UserDetailed` 共有フィールド
        // なので [`from_actor_detailed`] (基底) で実データを挿入済み。
        // `followedMessage` は Me / `UserDetailedNotMeWithRelations` のみが
        // 持つフィールド (base `UserDetailed` には無い) で、Sakurasato は
        // 後者の「相手との関係付き NotMe」emission 経路を持たないため、実質
        // ここ (self) でのみ実データを返す。
        map.insert(
            "followedMessage".to_string(),
            actor
                .followed_message
                .clone()
                .map_or(JsonValue::Null, JsonValue::String),
        );
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

/// `MissUserList` (Misskey 互換の `UserList`)。`users/lists/*` +
/// `notes/user-list-timeline` (= リスト機能, `crate::miauth::lists`) で使う。
///
/// Misskey 本家の `id` は他の型と同じく string ── ここでも
/// `user_list.id` (`i64`) を stringify する ([`MissUser::id`] と同じ流儀)。
/// `user_ids` は Misskey `UserList.userIds` (= optional だが実クライアントは
/// 参照するため常に emit する) に合わせて `Vec<String>` (stringified actor id)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissUserList {
    pub id: String,
    pub created_at: String,
    pub name: String,
    pub user_ids: Vec<String>,
}

/// [`sakurasato_core::model::UserListRow`] + メンバー actor id 一覧 → `MissUserList`。
pub fn user_list_to_miss(
    row: &sakurasato_core::model::UserListRow,
    member_ids: &[i64],
) -> MissUserList {
    MissUserList {
        id: row.id.to_string(),
        created_at: row
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        name: row.title.clone(),
        user_ids: member_ids.iter().map(i64::to_string).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sakurasato_core::model::{ActorField, ActorRow, EmojiRow};
    use sqlx::types::Json as SqlxJson;

    fn neutral_rel() -> crate::follow::FollowRelationship {
        crate::follow::FollowRelationship::neutral()
    }

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
            birthday: None,
            location: None,
            lang: None,
            followed_message: None,
            fields: SqlxJson(vec![]),
            followers_count: 0,
            following_count: 0,
            notes_count: 0,
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// local actor の `host` は **null** で返る (Misskey `UserLite` 慣行)。
    #[test]
    fn local_actor_host_is_null() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 3, 5, 7, BTreeMap::new());
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
        let miss = from_actor_and_counts(&actor, 0, 0, 0, BTreeMap::new());
        assert_eq!(miss.host.as_deref(), Some("remote.test"));
    }

    /// 鍵アカウント (`manually_approves_followers = true`) は `isLocked: true`。
    #[test]
    fn locked_actor_serializes_as_locked() {
        let actor = fake_actor(true, "sakurasato", true);
        let miss = from_actor_and_counts(&actor, 0, 0, 0, BTreeMap::new());
        assert!(miss.is_locked);
    }

    /// camelCase + null 表現が Misskey wire spec と一致することを serde で確認。
    #[test]
    fn json_shape_matches_misskey_userlite_minimum() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 11, 12, 13, BTreeMap::new());
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
        let miss = from_actor_and_counts(&actor, 0, 0, 0, BTreeMap::new());
        let json = serde_json::to_value(&miss).unwrap();
        assert!(json["name"].is_null());
        // **non-null**: icon_url が None でも identicon URL で fallback。
        assert!(json["avatarUrl"].is_string());
        let url = json["avatarUrl"].as_str().unwrap();
        assert!(url.starts_with("https://"));
        assert!(url.contains("/identicon/"));
    }

    /// バグ 1 の回帰ガード: 渡した `emojis` map が `UserLite` JSON の
    /// `emojis` object にそのまま emit される (空でも `{}` = Misskey wire は
    /// 常に object)。key はコロン無し shortcode。
    #[test]
    fn from_actor_and_counts_emits_emojis_map() {
        let actor = fake_actor(true, "sakurasato", false);
        let emojis = BTreeMap::from([
            (
                "sakura".to_string(),
                "https://sakurasato.test/media/emoji/local/sakura.webp".to_string(),
            ),
            (
                "blobcat".to_string(),
                "https://sakurasato.test/media/emoji/local/blobcat.webp".to_string(),
            ),
        ]);
        let miss = from_actor_and_counts(&actor, 0, 0, 0, emojis.clone());
        assert_eq!(miss.emojis, emojis);
        let json = serde_json::to_value(&miss).unwrap();
        assert_eq!(
            json["emojis"],
            serde_json::json!({
                "sakura": "https://sakurasato.test/media/emoji/local/sakura.webp",
                "blobcat": "https://sakurasato.test/media/emoji/local/blobcat.webp",
            })
        );
    }

    /// バグ 1 の回帰ガード: `UserDetailed` (= `users/show` 等) も渡した
    /// `emojis` map を emit する (基底 `from_actor_and_counts` 経由)。
    #[test]
    fn from_actor_detailed_emits_emojis_map() {
        let actor = fake_actor(false, "sakurasato.test", false);
        let emojis = BTreeMap::from([(
            "sakura".to_string(),
            "https://sakurasato.test/media/emoji/local/sakura.webp".to_string(),
        )]);
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), emojis.clone());
        assert_eq!(
            v["emojis"],
            serde_json::json!({ "sakura": "https://sakurasato.test/media/emoji/local/sakura.webp" })
        );
    }

    /// shortcode を何も含まない actor は `collect_actor_emoji_shortcodes` が
    /// 空を返す (= `resolve_user_emojis` の DB 引きを short-circuit する)。
    #[test]
    fn collect_actor_emoji_shortcodes_empty_when_no_shortcode() {
        let actor = fake_actor(true, "sakurasato", false);
        assert!(collect_actor_emoji_shortcodes(&actor).is_empty());
    }

    /// `display_name` / `summary` / `fields` (name + value) から `:foo:` を
    /// 横断的に集め、ASCII-lowercase + dedupe する (= 本文側と同じ規約)。
    #[test]
    fn collect_actor_emoji_shortcodes_spans_all_text_fields() {
        let mut actor = fake_actor(true, "sakurasato", false);
        actor.display_name = Some("Alice :Sakura: :blobcat:".into());
        actor.summary = Some("suki :sakura:".into());
        actor.fields = SqlxJson(vec![ActorField {
            name: "URL".into(),
            value: "example.com :blobcat:".into(),
        }]);
        // ASCII-lowercase + 全フィールド横断 + dedupe。
        assert_eq!(
            collect_actor_emoji_shortcodes(&actor),
            vec!["blobcat".to_string(), "sakura".to_string()]
        );
    }

    /// `emoji_rows_to_url_map`: local 行優先 + `image_key` 欠落行スキップ。
    #[test]
    fn emoji_rows_to_url_map_prefers_local_and_skips_missing_image() {
        let local = EmojiRow {
            id: 1,
            shortcode: "sakura".into(),
            host: None,
            category: None,
            aliases: SqlxJson(vec![]),
            image_key: Some("emoji/local/sakura.webp".into()),
            media_type: "image/webp".into(),
            ap_id: None,
            is_local: true,
            license: None,
            is_sensitive: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_failed_at: None,
        };
        // 同じ shortcode の learned-remote 行 (後ろに置いて「local 優先」を確認)。
        let mut remote = local.clone();
        remote.id = 2;
        remote.host = Some("misskey.io".into());
        remote.image_key = Some("emoji/remote/misskey.io/sakura.webp".into());
        remote.is_local = false;
        // image_key 欠落行 (= fetch 失敗キャッシュ行) はスキップ。
        let mut broken = local.clone();
        broken.id = 3;
        broken.shortcode = "broken".into();
        broken.image_key = None;

        let map = emoji_rows_to_url_map(&[broken, remote, local], "sakurasato.test");
        assert_eq!(
            map.len(),
            1,
            "broken 行はスキップ、remote は local に敗れる"
        );
        assert_eq!(
            map["sakura"],
            "https://sakurasato.test/media/emoji/local/sakura.webp"
        );
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
    fn build_miss_file_sets_thumbnail_url_for_images() {
        // 画像は `thumbnailUrl` に `url` をそのまま入れる ── Aria 等が
        // タイムラインのインラインサムネイル表示に使う。null だとサムネイルが
        // 出ず、タップ時の url (フル画像) しか開けない。
        let raw = json!({
            "url": "https://example.test/media/abc.webp",
            "mediaType": "image/webp",
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).unwrap();
        assert_eq!(
            f.thumbnail_url.as_deref(),
            Some("https://example.test/media/abc.webp"),
            "image attachments must carry a non-null thumbnailUrl"
        );
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["thumbnailUrl"], "https://example.test/media/abc.webp");
    }

    #[test]
    fn build_miss_file_thumbnail_url_null_for_non_images() {
        // 動画 / 音声は画像サムネイルを持たないので `thumbnailUrl` は null。
        let raw = json!({
            "url": "https://example.test/media/clip.mp4",
            "mediaType": "video/mp4",
        });
        let f = build_miss_file(&raw, "2026-06-03T00:00:00.000Z", false).unwrap();
        assert!(
            f.thumbnail_url.is_none(),
            "non-image attachments must keep thumbnailUrl null"
        );
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
                // Issue #135: cached remote ── 自鯖 versitygw キー形式。
                image_key: Some("emoji/remote/misskey.io/blob.webp".into()),
                media_type: Some("image/webp".into()),
                is_local: Some(false),
                first_at: Utc::now(),
            },
            ReactionSummaryRow {
                note_id: 1,
                content: ":legacy@old.test:".into(),
                count: 1,
                emoji_id: Some(9),
                // Issue #135: 旧 row (= remote URL 直入れ時代) は素通しで
                // graceful migration、再 upsert で `emoji/remote/...` に
                // 置き換わるまで動作を維持する。
                image_key: Some("https://old.test/files/legacy.webp".into()),
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
        // Issue #135: cached remote (= `emoji/remote/...`) も自鯖 /media/
        // URL に展開する。
        assert_eq!(
            emojis["blob@misskey.io"],
            "https://sakurasato.test/media/emoji/remote/misskey.io/blob.webp"
        );
        // 旧 row (URL 直入れ) は素通し。
        assert_eq!(
            emojis["legacy@old.test"],
            "https://old.test/files/legacy.webp"
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
        let v = from_actor_detailed(&actor, 1, 2, 3, neutral_rel(), BTreeMap::new());
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
        // misskey-dart `UserDetailedNotMe` の required bool 3 件。
        assert_eq!(v["isSilenced"], false);
        assert_eq!(v["isSuspended"], false);
        assert_eq!(v["publicReactions"], true);
        // follow relationship 4 件 (neutral なので全て false)。
        assert_eq!(v["isFollowing"], false);
        assert_eq!(v["isFollowed"], false);
        assert_eq!(v["hasPendingFollowRequestFromYou"], false);
        assert_eq!(v["hasPendingFollowRequestToYou"], false);
        // WithRelations 版の required bool + 既定値 (`isFollowing` を emit すると
        // misskey_dart は必ず `UserDetailedNotMeWithRelations` で parse する)。
        assert_eq!(v["isBlocking"], false);
        assert_eq!(v["isBlocked"], false);
        assert_eq!(v["isMuted"], false);
        assert_eq!(v["isRenoteMuted"], false);
        assert_eq!(v["notify"], "normal");
        assert_eq!(v["withReplies"], true);
    }

    #[test]
    fn from_actor_detailed_strips_html_from_remote_description() {
        // remote actor の `summary` は HTML。`description` は plain 化されて
        // 生タグが消える (= #271、Note 本文の #170 と同型)。
        let mut actor = fake_actor(false, "remote.test", false);
        actor.summary =
            Some(r#"<p>hello <a href="https://remote.test/@me">@me</a></p><p>line2</p>"#.into());
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), BTreeMap::new());
        assert_eq!(v["description"], "hello @me\n\nline2");
    }

    #[test]
    fn from_actor_detailed_keeps_local_plain_description_verbatim() {
        // local actor の `summary` は plain text。`<` を含んでも strip されず
        // そのまま (html_to_plain_text を通さない)。
        let mut actor = fake_actor(true, "sakurasato.test", false);
        actor.summary = Some("price < 100 & rising".into());
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), BTreeMap::new());
        assert_eq!(v["description"], "price < 100 & rising");
    }

    /// misskey-dart `_$UserDetailedNotMeFromJson` で **非 null / default 無し**
    /// に読まれる field が一つでも欠けると Aria が `MisskeyUsers.show` で crash
    /// する。`/api/users/show` / `/api/users/{following,followers}` が共有する
    /// [`from_actor_detailed`] の出力に、それら required field が漏れなく載って
    /// いることを固定する回帰テスト (= #174 の `UserDetailed` 版)。
    #[test]
    fn from_actor_detailed_emits_all_required_userdetailednotme_fields() {
        // remote actor (icon_url 無し) でも avatarUrl が non-null になる経路。
        let actor = fake_actor(false, "remote.test", false);
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), BTreeMap::new());
        // string / number で `as String` / `as num` 直読みされ、null だと throw。
        assert!(v["id"].is_string(), "id must be a string");
        assert!(v["username"].is_string(), "username must be a string");
        assert!(
            v["avatarUrl"].as_str().is_some_and(|s| !s.is_empty()),
            "avatarUrl must be a non-null string (identicon fallback)"
        );
        assert!(v["createdAt"].is_string(), "createdAt must be a string");
        assert!(v["followersCount"].is_number());
        assert!(v["followingCount"].is_number());
        assert!(v["notesCount"].is_number());
        // `as bool` 直読みされる required bool 群。
        for key in [
            "isBot",
            "isCat",
            "isLocked",
            "isSilenced",
            "isSuspended",
            "publicReactions",
            "isFollowing",
            "isFollowed",
            "hasPendingFollowRequestFromYou",
            "hasPendingFollowRequestToYou",
            "isBlocking",
            "isBlocked",
            "isMuted",
            "isRenoteMuted",
        ] {
            assert!(
                v[key].is_boolean(),
                "required bool `{key}` must be present and boolean"
            );
        }
    }

    /// `UserDetailedNotMeWithRelations` は `notify` / `withReplies` も
    /// wire 上よく読まれる。nullable なので欠けても crash しないが、
    /// Misskey 既定値が載っていることを lock する。
    #[test]
    fn from_actor_detailed_emits_withrelations_defaults() {
        let actor = fake_actor(false, "remote.test", false);
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), BTreeMap::new());
        assert_eq!(v["notify"], "normal");
        assert_eq!(v["withReplies"], true);
    }

    /// follow relationship 4 フィールドの **取り違え (swap)** 検出 ──
    /// `isFollowing` / `isFollowed` / `hasPendingFollowRequestFromYou` /
    /// `hasPendingFollowRequestToYou` は名前が似ていて swap しやすいので、
    /// 全 true の `FollowRelationship` を渡してキーごとに個別 assert する。
    #[test]
    fn from_actor_detailed_emits_follow_relationship_without_swapping() {
        let actor = fake_actor(false, "remote.test", false);
        let rel = crate::follow::FollowRelationship {
            following: true,
            follow_state: Some(sakurasato_core::model::FollowState::Accepted),
            followed_by: true,
            follow_id: Some(7),
            has_pending_follow_request_from_you: true,
            has_pending_follow_request_to_you: true,
        };
        let v = from_actor_detailed(&actor, 0, 0, 0, rel, BTreeMap::new());
        assert_eq!(
            v["isFollowing"], true,
            "isFollowing must come from `following`"
        );
        assert_eq!(
            v["isFollowed"], true,
            "isFollowed must come from `followed_by`"
        );
        assert_eq!(
            v["hasPendingFollowRequestFromYou"], true,
            "must come from `has_pending_follow_request_from_you`"
        );
        assert_eq!(
            v["hasPendingFollowRequestToYou"], true,
            "must come from `has_pending_follow_request_to_you`"
        );
    }

    /// 2026-07-22 調査: `misskey_dart` の `User.fromJson` (=
    /// `MisskeyUsers.search` / `searchByUsernameAndHost` が使う) は
    /// `json.containsKey("url")` の有無だけで `UserLite` / `UserDetailed` を
    /// 出し分ける。`url` キー自体が無いと常に `UserLite` 扱いになり、Aria の
    /// `SearchUsersNotifier` が `whereType<UserDetailed>()` で全件除外 →
    /// 200 + 非空 JSON なのに検索結果が常に空、という回帰が起きる。値は
    /// `null` でよいが **キーは必ず存在すること**。
    #[test]
    fn from_actor_detailed_includes_url_key_for_userdetailed_dispatch() {
        let actor = fake_actor(false, "remote.test", false);
        let v = from_actor_detailed(&actor, 0, 0, 0, neutral_rel(), BTreeMap::new());
        let map = v.as_object().expect("from_actor_detailed must be object");
        assert!(
            map.contains_key("url"),
            "response must contain a `url` key (even if null) so misskey_dart's \
             User.fromJson dispatches to UserDetailed instead of UserLite: {v}"
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
            my_reaction: None,
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
        let note = timeline_entry_to_miss_note(&entry, &summary, "sakurasato.test", &EMPTY_EMOJIS);
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
        let note = timeline_entry_to_miss_note(&entry, &summary, "sakurasato.test", &EMPTY_EMOJIS);
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
        let note = timeline_entry_to_miss_note(&entry, &summary, "example.com:8443", &EMPTY_EMOJIS);
        let json = serde_json::to_value(&note).unwrap();
        assert!(
            json["user"]["host"].is_null(),
            "dev local_host with port must still emit user.host: null"
        );
    }
}
