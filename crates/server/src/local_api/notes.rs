//! `POST /api/v1/notes` ── ローカル投稿の作成 + Create Activity 配送。
//!
//! ## 流れ
//!
//! 1. ローカル actor (`config.server.user`) を解決する。`init` 未実行なら 503。
//! 2. リクエスト body をバリデーションし、`note` 行を tx 内で 1 行挿入する。
//!    BIGSERIAL の `id` 採番後に `ap_id` と `url` を canonical URL に書き戻す。
//! 3. visibility に応じて to/cc を組み立て、Create Activity を生成する。
//! 4. `repo::follow::list_accepted_inboxes` で配送先 inbox 集合を取り、
//!    inbox ごとに `delivery::enqueue_activity` で 1 行 push する。常駐
//!    worker が後で実配送する。
//! 5. broadcast channel に `note.created` を publish する。SSE 接続中の
//!    TUI が即時受け取れるようにする。
//! 6. 201 Created + Location ヘッダ + 作成後の JSON サマリを返す。
//!
//! ## バリデーション
//!
//! - `content`: 必須、非空。**長さ上限 5000 文字** ── Mastodon の既定
//!   500 字より大きいが、Misskey 系の慣習 (3000) より少し甘い。お一人様
//!   サーバなので暫定値、後で `config` で調整可能にする予定。
//! - `summary` (CW): 任意、長さ上限 200 文字。
//! - `visibility`: `public` / `unlisted` / `followers` / `direct` のいずれか。
//!   **#65**: `direct` は content から `@user@host` mention を抽出して
//!   その actor の inbox にのみ配送する (followers 不要)。direct で mention が
//!   1 件も解決できなければ 400。
//! - `in_reply_to_ap_id`: 任意。`url::Url::parse` で `http`/`https` のみ
//!   受け入れる。host 必須。**#64**: 返信先 Note を DB から引き、その
//!   `attributedTo` actor を `cc` (direct なら `to`) に追加し、remote なら
//!   親 author の inbox も `delivery_queue` に積む ── 未フォロー相手への
//!   返信が連合相手から「気付かれない」既知バグの解消。
//! - **#65 mention**: `content` から `@user@host` を抽出し、各 actor を
//!   `repo::actor::get_by_username_host` → media-proxy `WebFinger` →
//!   `remote_actor::fetch_and_upsert` の順で解決する。解決した actor URI を
//!   visibility に応じて `to` (direct) または `cc` (それ以外) に乗せ、`tag`
//!   配列に `Mention` を追加し、remote 相手なら inbox を `delivery_queue` に
//!   積む。解決失敗 (= `WebFinger` / fetch エラー) は **400** で投稿全体を拒否する
//!   (= mention 1 件でも届かないと「気付かれない」連合相手が出るため)。
//!
//! ## エラー
//!
//! - 400: バリデーション失敗 (content 空 / 長すぎ / visibility 不正 / mention
//!   解決失敗 / direct で宛先 0 件 等)
//! - 503: ローカル actor が無い (`init` 未実行) / DB アクセス失敗

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use sakurasato_core::model::{ActorRow, MediaRow, Visibility};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::delivery;
use crate::local_api::media::{attachment_document, build_media_url};
use crate::local_api::stream::{NoteCreatedPayload, TimelineEvent};
use crate::media_proxy_client::MediaProxyError;
use crate::remote_actor::{self, FetchError};
use crate::state::AppState;
use crate::webfinger_guard;

const CONTENT_MAX: usize = 5_000;
const SUMMARY_MAX: usize = 200;
const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";
/// 添付の最大件数。Mastodon API の 4 件と揃える ── 連合相手にも違和感が
/// 出ない値で、お一人様サーバとしても十分。
const ATTACHMENT_MAX: usize = 4;
/// **#65**: 1 投稿で resolve する mention の最大件数。お一人様 server で
/// ローカル API のアクセス権 = 所有者本人のため自己 `DoS` が中心だが、
/// 5000 文字 content + `@a@b.cd` (8 文字) で最大 ~625 件まで通る計算になり、
/// それぞれ `WebFinger` + actor fetch を直列実行すると応答が分単位になる。
/// Mastodon の慣習に近い 50 件を上限とする (PR #78 review #2)。
const MENTION_MAX: usize = 50;
/// 1 投稿あたりの local emoji shortcode 上限。content から `:foo:` を抽出した
/// 後の dedupe 済み件数で評価。Misskey の慣習 (= 1 投稿 30 個前後) に余裕を
/// もたせて 64 個まで許可、超えたぶんは static drop。
const EMOJI_MAX: usize = 64;

#[derive(Debug, Deserialize)]
pub struct CreateNoteRequest {
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub sensitive: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub in_reply_to_ap_id: Option<String>,
    /// M7: 添付メディアの `media.id` 配列。事前に
    /// `POST /api/v1/media?kind=attachment` で上げておいた行を指す。
    /// 重複は除去され、最大 [`ATTACHMENT_MAX`] 件 (Mastodon と揃えて 4)。
    #[serde(default)]
    pub attachment_ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateNoteResponse {
    pub id: i64,
    pub ap_id: String,
    pub url: String,
    pub content: String,
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub published_at: chrono::DateTime<chrono::Utc>,
    /// この作成で `delivery_queue` に挿入された行数 (= 配送試行予定の inbox 数)。
    /// 0 = フォロワー無し or public のみで配送不要 (= 連合先がいない)。
    pub queued_deliveries: usize,
}

#[allow(clippy::too_many_lines)]
pub async fn create(State(state): State<AppState>, Json(req): Json<CreateNoteRequest>) -> Response {
    let visibility = match parse_visibility(req.visibility.as_deref()) {
        Ok(v) => v,
        Err(reason) => return bad_request(reason),
    };
    if let Err(reason) = validate_request(&req) {
        return bad_request(reason);
    }

    let local_actor = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(LocalActorError::Missing) => {
            return error_with_body(
                StatusCode::SERVICE_UNAVAILABLE,
                "local actor not initialized; run `sakurasato init`",
            );
        }
        Err(LocalActorError::Db(err)) => {
            error!(?err, "POST /api/v1/notes: resolve local actor failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // M7: 添付メディアを先に DB から引いて、所有者・kind・未紐付けを確認する。
    // 同一 tx で attach するので Vec<MediaRow> をここで握っておき、tx 内で
    // attach_to_note を呼ぶ。
    let attachments = match load_attachments(&state, &local_actor, &req.attachment_ids).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // **#64**: `in_reply_to_ap_id` が指す親 Note を pool で引き、(parent_note_id,
    // parent_author_uri, parent_author_inbox) を組む。`persist_note` が tx 内
    // で再 lookup する fallback も残してあるので、ここで取れなくても note_id
    // 紐付けは諦めない (自己 reply の race 救済)。
    let reply_parent = match req.in_reply_to_ap_id.as_deref() {
        Some(uri) => resolve_reply_parent(&state, &local_actor, uri).await,
        None => None,
    };

    // **#65**: content から `@user@host` を抽出し、各 actor を解決する。
    // direct visibility の宛先決定にも、public/unlisted/followers の cc
    // に乗せる mention 通知にも、両方で使う共通経路。
    let parsed_mentions = parse_mentions(&req.content);
    if parsed_mentions.len() > MENTION_MAX {
        return bad_request_owned(&format!(
            "content has {} mentions; the maximum per post is {MENTION_MAX}",
            parsed_mentions.len(),
        ));
    }
    let mentions = match resolve_mentions(&state, &local_actor, &parsed_mentions).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // **Issue #102**: content から `:foo:` 形式の local emoji shortcode を
    // 抽出し、DB の `emoji` 行に解決して AP `Emoji` tag 配列を組み立てる。
    // 解決できなかった shortcode は黙って drop (= 連合相手に絵文字を表示
    // させる手段が無いため tag に乗せない。content の `:foo:` テキストは
    // そのまま残るので「shortcode 風文字列」として表示される)。
    let emoji_tags = resolve_emoji_tags(&state, &req.content).await;

    // **#65**: direct visibility は宛先解決が完了して初めて成立する。
    // 解決後 mention 0 件 + reply_parent も無い場合は配送先がゼロになるので
    // 400 で拒否する (= followers にも配らない = どこにも届かない post)。
    // `mentions.is_empty()` が真なら `inbox_for_delivery.is_none()` 系の
    // all() 条件は真空的に true なので、空判定だけで十分 (PR #78 review #1)。
    if matches!(visibility, Visibility::Direct) && mentions.is_empty() && reply_parent.is_none() {
        return bad_request(
            "direct visibility requires at least one resolvable @user@host mention or a reply target",
        );
    }

    let published_at = Utc::now();
    let prepared = PreparedNote::from_request(
        &req,
        &local_actor,
        visibility,
        &attachments,
        reply_parent.as_ref(),
        &mentions,
        emoji_tags,
        &state,
    );

    // direct で `to` が空 (= 自己 mention のみで剥がれて何も残らない等) なら拒否。
    if matches!(visibility, Visibility::Direct) && prepared.to.is_empty() {
        return bad_request("direct visibility has no recipients after resolution");
    }

    let Ok(inserted) = persist_note(
        &state,
        &local_actor,
        &req,
        &prepared,
        visibility,
        published_at,
        &attachments,
        reply_parent.as_ref(),
    )
    .await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let canonical_url = format!(
        "https://{host}/notes/{id}",
        host = state.config().server.host,
        id = inserted,
    );

    let activity = build_create_activity(
        &local_actor,
        &canonical_url,
        &req.content,
        prepared.summary.as_deref(),
        prepared.sensitive,
        req.language.as_deref(),
        req.in_reply_to_ap_id.as_deref(),
        &prepared.to,
        &prepared.cc,
        &prepared.attachment_documents,
        &prepared.all_tags(),
        published_at,
    );
    // **#65**: extra_inboxes は (a) 返信先 author の inbox と (b) mention で
    // 解決した remote actor の inbox の和集合。重複は `enqueue_deliveries` 側で
    // followers と合わせて dedupe する。
    let mut extra_inboxes: Vec<String> = reply_parent
        .as_ref()
        .and_then(|p| p.inbox_for_delivery.clone())
        .into_iter()
        .collect();
    for m in &mentions {
        if let Some(inbox) = m.inbox_for_delivery.as_deref()
            && !extra_inboxes.iter().any(|i| i == inbox)
        {
            extra_inboxes.push(inbox.to_string());
        }
    }
    // **#65**: direct は followers に流さない (= mention 先のみに配送)。
    // public/unlisted/followers は従来どおり followers 集合 + extra を配送。
    let skip_followers = matches!(visibility, Visibility::Direct);
    let queued = enqueue_deliveries(
        &state,
        &local_actor,
        &activity,
        &extra_inboxes,
        skip_followers,
    )
    .await;

    // SSE 接続中の TUI に push。subscriber 0 は send が Err になるが正常。
    let event = TimelineEvent::NoteCreated(Box::new(NoteCreatedPayload {
        id: inserted,
        ap_id: canonical_url.clone(),
        actor_id: local_actor.id,
        actor_ap_id: local_actor.ap_id.clone(),
        actor_preferred_username: local_actor.preferred_username.clone(),
        actor_display_name: local_actor.display_name.clone(),
        actor_icon_url: local_actor.icon_url.clone(),
        content: req.content.clone(),
        summary: prepared.summary.clone(),
        visibility: visibility.as_str().to_string(),
        sensitive: prepared.sensitive,
        url: Some(canonical_url.clone()),
        published_at,
    }));
    let _ = state.timeline_sender().send(event);

    let body = CreateNoteResponse {
        id: inserted,
        ap_id: canonical_url.clone(),
        url: canonical_url,
        content: req.content,
        summary: prepared.summary,
        visibility: visibility.as_str().to_string(),
        sensitive: prepared.sensitive,
        published_at,
        queued_deliveries: queued,
    };

    let location = format!("/notes/{inserted}");
    let mut response = (StatusCode::CREATED, Json(body)).into_response();
    if let Ok(hv) = HeaderValue::from_str(&location) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("location"), hv);
    }
    response
}

/// バリデーション後の入力を事前計算したまとまり。`create` 本体が薄くなる。
struct PreparedNote {
    summary: Option<String>,
    sensitive: bool,
    to: Vec<String>,
    cc: Vec<String>,
    /// `note.attachments` JSONB に書き込む AP Document 配列。
    /// `build_create_activity` にも渡して `Note.attachment` に同値を載せる。
    attachment_documents: Vec<JsonValue>,
    /// **#65**: `Note.tag` に乗せる Mention エントリ。`{type, href, name}` を
    /// 解決済み mention 1 件につき 1 つ。
    mention_tags: Vec<JsonValue>,
    /// **#102**: `Note.tag` に乗せる Emoji エントリ。content から抽出した
    /// `:foo:` shortcode を local emoji 行に解決して 1 件ごとに組み立てる。
    emoji_tags: Vec<JsonValue>,
}

impl PreparedNote {
    #[allow(clippy::too_many_arguments, reason = "post fixup を 1 関数にまとめる")]
    fn from_request(
        req: &CreateNoteRequest,
        actor: &ActorRow,
        visibility: Visibility,
        attachments: &[MediaRow],
        reply_parent: Option<&ReplyParentInfo>,
        mentions: &[ResolvedMention],
        emoji_tags: Vec<JsonValue>,
        state: &AppState,
    ) -> Self {
        let followers_url = actor
            .followers_url
            .clone()
            .unwrap_or_else(|| format!("{}/followers", actor.ap_id));
        let mention_uris: Vec<&str> = mentions.iter().map(|m| m.actor_uri.as_str()).collect();
        let (to, cc) = recipients_for(
            visibility,
            &followers_url,
            reply_parent.map(|p| p.actor_uri.as_str()),
            &mention_uris,
        );
        let host = &state.config().server.host;
        let attachment_documents = attachments
            .iter()
            .map(|m| attachment_document(host, m))
            .collect();
        let mut mention_tags: Vec<JsonValue> = mentions
            .iter()
            .map(|m| {
                json!({
                    "type": "Mention",
                    "href": m.actor_uri,
                    "name": m.name,
                })
            })
            .collect();
        // **#98**: reply_parent author を `tag.Mention` に自動追加する。
        // Mastodon の `process_audience` は audience に居て `tag.Mention` に
        // 無い account を silent mention として扱い、直前で `direct` 判定
        // していた visibility を `:limited` に降格する (API では `private` 表示)。
        // 親 author を明示 Mention として乗せることで、direct reply が正しく
        // direct のまま伝わり、非 direct でも親 author が「explicit mention」
        // として通知される (Mastodon / Misskey の慣習どおり)。
        //
        // 自己 reply (= `is_local_self`) と、content `@user@host` で既に
        // 解決済みの actor (= mentions と URI が同じ) はスキップ。
        if let Some(p) = reply_parent
            && !p.is_local_self
            && !mention_tags
                .iter()
                .any(|t| t.get("href").and_then(JsonValue::as_str) == Some(p.actor_uri.as_str()))
        {
            mention_tags.push(json!({
                "type": "Mention",
                "href": p.actor_uri,
                "name": p.mention_name,
            }));
        }
        Self {
            summary: req
                .summary
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            sensitive: req.sensitive.unwrap_or(false),
            to,
            cc,
            attachment_documents,
            mention_tags,
            emoji_tags,
        }
    }

    /// `Note.tag` 用に mention + emoji を結合した配列。`build_create_activity`
    /// にも `note.tags` JSONB にも同値を流し込む。
    fn all_tags(&self) -> Vec<JsonValue> {
        let mut v = Vec::with_capacity(self.mention_tags.len() + self.emoji_tags.len());
        v.extend(self.mention_tags.iter().cloned());
        v.extend(self.emoji_tags.iter().cloned());
        v
    }
}

/// `in_reply_to_ap_id` で指された親 Note と、その作者の配送情報。
///
/// `actor_uri` は to/cc に乗せる用 (local actor 自身でも乗せる ── 害は無く、
/// Mastodon/Misskey の慣習に合う)。`inbox_for_delivery` は **remote actor かつ
/// 自分以外** のときだけ Some ── local や自分自身の inbox は配送しない。
///
/// **#64 F-1 (Followers 可視性の意図)**: `Visibility::Followers` の返信でも
/// 親 author の inbox には配送する。これは Mastodon / Misskey / Nekonoverse
/// と同じ慣習で、「mentioned (cc に乗った) リモート actor は follow 関係
/// 問わず受領する」という `ActivityPub` の標準的な解釈。受領側は AS2 visibility
/// (= to/cc/audience) を見て「followers-only として表示」を選択するため、
/// 公開範囲はクライアント側で正しく扱われる。
///
/// **#64 F-4 (TOCTOU 受容)**: `note_id` は `state.pool()` で tx 外で取得し、
/// `persist_note` 内の INSERT に渡す。並行 Delete dispatch が tx 開始前に親
/// note を削除すると FK 違反で 503 を返す。お一人様サーバ + Delete は稀
/// なため、503 → ユーザ retry の経路で許容する。pre-resolve を回避したい
/// 場合は `reply_parent = None` を渡せば `persist_note` 内の tx 内 fallback が
/// 走り、tx 内検索 → 親が消えていれば `note_id = None` で続行できる。
#[derive(Debug, Clone)]
struct ReplyParentInfo {
    note_id: i64,
    actor_uri: String,
    /// **#98**: 親 author を `tag.Mention` に積むときの `@user@host` 表現。
    /// Mastodon の `process_audience` は audience に居て `tag.Mention` に無い
    /// account を **silent mention** として扱い、direct を `:limited` に降格
    /// する (= API では `private` 表示)。reply 経路でこれが起きないよう、
    /// 親 author 行から構築した `@user@host` を Mention.name に乗せる。
    mention_name: String,
    /// 親 author == ローカル actor (= 自己 reply) のとき `true`。`tag.Mention`
    /// 自動追加対象から外す ── 自分宛の Mention は受信側に不要なノイズ。
    is_local_self: bool,
    inbox_for_delivery: Option<String>,
}

/// 返信先 Note を pool で引き、(`note_id`, `actor_uri`, inbox) を組む。**#64**
///
/// 取れなければ `None`。失敗 (DB エラー / 未知 Note) はログだけ残して返信
/// 関係の追加配送をしない fallback で続行する ── 親 author が誰か分から
/// ないので「親 author 配送」自体が不能だが、投稿自体は通常経路で配送する。
async fn resolve_reply_parent(
    state: &AppState,
    local_actor: &ActorRow,
    parent_ap_id: &str,
) -> Option<ReplyParentInfo> {
    let parent_note = match repo::note::get_by_ap_id(state.pool(), parent_ap_id).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            warn!(%parent_ap_id, "reply parent note unknown locally; skipping reply-parent delivery");
            return None;
        }
        Err(err) => {
            warn!(?err, %parent_ap_id, "reply parent lookup failed");
            return None;
        }
    };
    let parent_actor = match repo::actor::get_by_id(state.pool(), parent_note.actor_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            warn!(
                %parent_ap_id,
                parent_actor_id = parent_note.actor_id,
                "reply parent actor row missing (orphaned note)",
            );
            return None;
        }
        Err(err) => {
            warn!(?err, %parent_ap_id, "reply parent actor lookup failed");
            return None;
        }
    };
    // 自分自身 / local actor 宛は配送しない (自己 inbox loop は net_guard でも
    // 弾かれるが、無駄な enqueue を避ける)。to/cc には残しておく ── 慣習どおり。
    let inbox_for_delivery = if parent_actor.id == local_actor.id || parent_actor.is_local {
        None
    } else {
        Some(
            parent_actor
                .shared_inbox_url
                .clone()
                .unwrap_or_else(|| parent_actor.inbox_url.clone()),
        )
    };
    let mention_name = format!("@{}@{}", parent_actor.preferred_username, parent_actor.host);
    let is_local_self = parent_actor.id == local_actor.id;
    Some(ReplyParentInfo {
        note_id: parent_note.id,
        actor_uri: parent_actor.ap_id,
        mention_name,
        is_local_self,
        inbox_for_delivery,
    })
}

/// **#65**: content から抽出した `@user@host` 1 件分。`name` は display 用に
/// 元のケースを保ったまま `"@user@host"` の形にしてあり、解決後の AS2
/// `Mention.name` にそのまま使う。`user` / `host` はサーバ側の lookup 用 ──
/// 大文字小文字違いは [`parse_mentions`] 側で正規化して dedupe する。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionAcct {
    user: String,
    host: String,
    /// `@user@host` の display 形 (元のケース保持)。AS2 `Mention.name` 用。
    name: String,
}

/// **#65**: `WebFinger` / DB lookup を通して actor に紐付けた mention 1 件分。
///
/// `inbox_for_delivery` は配送先 inbox。**自分自身 / 他の local actor** 宛は
/// 配送しないので `None`。remote actor は `shared_inbox` 優先で 1 件持つ。
/// to/cc には `actor_uri` を、`tag` には `(actor_uri, name)` の pair を載せる。
#[derive(Debug, Clone)]
struct ResolvedMention {
    actor_uri: String,
    name: String,
    inbox_for_delivery: Option<String>,
}

/// content から `:foo:` 形式の local emoji shortcode を抽出する。
///
/// 制約 (`repo::emoji::is_valid_shortcode` と同じ): ASCII alphanumeric +
/// underscore + hyphen、長さ 1..=128 (Issue #188 で 64 → 128 緩和、Misskey
/// 互換)。`:foo@host:` のリモート絵文字は本 PR では対象外で、shortcode に
/// `@` が来た時点で抽出を打ち切る (別 issue で対応する)。
///
/// 重複 shortcode は ASCII-lowercase で dedupe。上限 [`EMOJI_MAX`] を超えた
/// ぶんは drop (= attack 防御 + 投稿サイズ抑制)。
fn parse_emoji_shortcodes(content: &str) -> Vec<String> {
    let bytes = content.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut i = 0;
    while i < bytes.len() && out.len() < EMOJI_MAX {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() {
            let b = bytes[j];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                j += 1;
            } else {
                break;
            }
        }
        // 終端 `:` が必要、空 shortcode (`::`) は無視、長さ 1..=128 制約
        // (Issue #188 で 64 → 128 緩和、`is_valid_shortcode` と整合)。
        if j == start || j >= bytes.len() || bytes[j] != b':' {
            i += 1;
            continue;
        }
        let len = j - start;
        if !(1..=128).contains(&len) {
            i += 1;
            continue;
        }
        let shortcode = &content[start..j];
        let lc = shortcode.to_ascii_lowercase();
        if seen.insert(lc.clone()) {
            out.push(lc);
        }
        // 終端 `:` の次から再開 ── `:a::b:` のような連続書きも拾えるように。
        i = j + 1;
    }
    out
}

/// 抽出した shortcode を DB の **local emoji 行** に解決し、AP `Emoji`
/// tag JSON 配列を返す。
///
/// - 解決できなかった shortcode は黙って drop (= 連合相手側で `:foo:` の
///   テキストはそのまま見えるが画像化はされない、許容範囲)。
/// - DB エラーも drop (= 投稿全体を 503 にする筋でもないので)。
async fn resolve_emoji_tags(state: &AppState, content: &str) -> Vec<JsonValue> {
    let shortcodes = parse_emoji_shortcodes(content);
    if shortcodes.is_empty() {
        return Vec::new();
    }
    let host = state.config().server.host.clone();
    let mut out: Vec<JsonValue> = Vec::with_capacity(shortcodes.len());
    for sc in shortcodes {
        match repo::emoji::get_local_by_shortcode(state.pool(), &sc).await {
            Ok(Some(row)) => {
                let url = build_media_url(&host, &row.image_key);
                let emoji_ap_id = format!("https://{host}/emojis/{sc}");
                out.push(json!({
                    "type": "Emoji",
                    "id": emoji_ap_id,
                    "name": format!(":{sc}:"),
                    "updated": row.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "icon": {
                        "type": "Image",
                        "mediaType": row.media_type,
                        "url": url,
                    },
                }));
            }
            Ok(None) => {
                // shortcode が DB に無い ── テキストとしてそのまま残す。
            }
            Err(err) => {
                warn!(?err, shortcode = %sc, "emoji shortcode resolution failed; dropping tag");
            }
        }
    }
    out
}

/// content から `@user@host` を抽出する。byte 単位の単純スキャナで、外部
/// 依存を増やさずに済ませる ── `regex` を入れるほどの複雑度は無く、
/// AP 連合で実際に飛んでくる acct は ASCII の `[A-Za-z0-9_.-]` + host が
/// `[A-Za-z0-9.-]` で十分カバーされる。
///
/// 単語境界の判定: `@` の直前が ASCII 英数 / `_` / `@` のどれでもないとき
/// だけ mention 開始とみなす ── これで `bob@example.com` のような
/// メールアドレス文字列を mention と誤認しない。
///
/// 末尾の `.` は文末ピリオドを誤って host に含めないよう trim する
/// (例: `Hi @bob@example.com.` → host `example.com`)。
///
/// 同じ acct を複数回書いても結果は **1 件のみ** (= ASCII-lower で dedupe)。
fn parse_mentions(content: &str) -> Vec<MentionAcct> {
    let bytes = content.as_bytes();
    let mut out: Vec<MentionAcct> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut i = 0;
    // **PR #78 review F-2**: 直前の反復で mention を抽出しきった場合、その
    // mention の末尾は host TLD 文字 (= 英数) なので単純な前一文字判定では
    // 次の `@user@host` が word boundary 違反として黙って捨てられる。
    // `@alice@a.test@bob@b.test` を空白なしで書かれてもどちらも拾うため、
    // 「直前位置が前回 mention の終端 (= k)」も boundary と認める。
    let mut prev_mention_end: Option<usize> = None;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            prev_mention_end = None;
            i += 1;
            continue;
        }
        // 単語境界: `@` の直前が英数 / `_` / `@` ならスキップ (メアド誤認回避)。
        // 非 ASCII バイト (= マルチバイト UTF-8 の途中) は ascii_alphanumeric() が
        // false を返すので「日本語の後の @user@host」は正しく拾える。
        // ただし「直前位置が前回 mention 末尾」のときは boundary と認める (F-2)。
        let prev_ok = i == 0 || prev_mention_end == Some(i) || {
            let b = bytes[i - 1];
            !b.is_ascii_alphanumeric() && b != b'_' && b != b'@'
        };
        prev_mention_end = None;
        if !prev_ok {
            i += 1;
            continue;
        }
        let user_start = i + 1;
        let mut j = user_start;
        while j < bytes.len() {
            let b = bytes[j];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' {
                j += 1;
            } else {
                break;
            }
        }
        // user 末尾の `.` は host 区切り `@` の手前で trim する (= `foo.@host`
        // 系の異常入力をはじく)。
        let mut user_end = j;
        while user_end > user_start && bytes[user_end - 1] == b'.' {
            user_end -= 1;
        }
        if user_end == user_start || j >= bytes.len() || bytes[j] != b'@' {
            i += 1;
            continue;
        }
        let host_start = j + 1;
        let mut k = host_start;
        while k < bytes.len() {
            let b = bytes[k];
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' {
                k += 1;
            } else {
                break;
            }
        }
        // 文末ピリオドを host から削る (例: `... @bob@example.com.` の最後の `.`)。
        let mut host_end = k;
        while host_end > host_start && bytes[host_end - 1] == b'.' {
            host_end -= 1;
        }
        // host は最低 1 つの `.` を含むこと (= TLD の存在で domain らしさを担保)。
        let host_slice = &bytes[host_start..host_end];
        if host_slice.is_empty() || !host_slice.contains(&b'.') {
            i += 1;
            continue;
        }
        // ここに来た時点で user/host は ASCII バイトのみ → str スライスは安全。
        let user_str = content[user_start..user_end].to_string();
        let host_str = content[host_start..host_end].to_string();
        let key = (user_str.to_ascii_lowercase(), host_str.to_ascii_lowercase());
        if seen.insert(key) {
            let name = format!("@{user_str}@{host_str}");
            out.push(MentionAcct {
                user: user_str,
                host: host_str,
                name,
            });
        }
        i = k;
        prev_mention_end = Some(k);
    }
    out
}

/// 解析した mention 一覧を actor に解決する。
///
/// 解決順:
/// 1. `repo::actor::get_by_username_host(user, host)` で DB ヒットを試す。
/// 2. 居なければ `state.media_proxy().resolve_webfinger("user@host")` で
///    `WebFinger` 経由 actor URI を得る。
/// 3. `remote_actor::fetch_and_upsert` で actor JSON を取得して DB 行を確保。
///
/// **失敗 = 400**: `WebFinger` / fetch エラーは mention 通知が「気付かれない」
/// 連合相手を生むので、投稿全体を **400** で reject して呼び出し側に再試行を
/// 委ねる (= 部分配送で「届いた気になる」より、失敗を明確に返す方が安全)。
///
/// **自己 mention は drop**: `actor.id == local_actor.id` の解決結果は
/// to/cc にも tag にも乗せない (= 自分宛 DM を相手側から「変な inbox loop」と
/// 認識される回避)。
///
/// **重複 dedupe**: 解決後の actor URI 単位で 1 件のみ残す ── 同じ actor を
/// 2 系統の acct (= alias 違い等) で書かれても to/cc が膨らまない。
async fn resolve_mentions(
    state: &AppState,
    local_actor: &ActorRow,
    mentions: &[MentionAcct],
) -> Result<Vec<ResolvedMention>, Response> {
    let mut out: Vec<ResolvedMention> = Vec::with_capacity(mentions.len());
    let mut seen_uri: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in mentions {
        let actor = match resolve_mention_actor(state, m).await {
            Ok(a) => a,
            Err(reason) => return Err(bad_request_owned(&reason)),
        };
        if actor.id == local_actor.id {
            // 自己 mention は配送先ゼロでも no-op (= post 本文には残るが
            // to/cc/tag には乗らない)。
            continue;
        }
        if !seen_uri.insert(actor.ap_id.clone()) {
            continue;
        }
        // 他の local actor (お一人様サーバなので原則出ない) は inbox 配送せず、
        // to/cc には actor URI を残す ── AP 上は「通知済み」と扱える。
        let inbox_for_delivery = if actor.is_local {
            None
        } else {
            Some(
                actor
                    .shared_inbox_url
                    .clone()
                    .unwrap_or_else(|| actor.inbox_url.clone()),
            )
        };
        out.push(ResolvedMention {
            actor_uri: actor.ap_id,
            name: m.name.clone(),
            inbox_for_delivery,
        });
    }
    Ok(out)
}

/// 1 件分の `@user@host` を actor 行に変換する。
///
/// `enable_remote_fetch=false` (テスト経路) のときは `WebFinger` / fetch を
/// 試さず DB ヒットだけで判定する ── 統合テストは事前に
/// `repo::actor::insert` で actor を seed する契約。
///
/// **PR #78 review F-1 (cross-domain hijack 防御)**: `WebFinger` が返した
/// `actor_uri` のホストが、クエリしたホスト (`m.host`) と一致するか検証する。
/// 一致しない場合、悪意ある `WebFinger` サーバが「`@legit@evil.example` を
/// `https://victim.example/users/legit` に向ける」差し替えをやって DM 宛先を
/// 乗っ取れる。`fetch_and_upsert` 内の `id == ap_id` 自己整合性チェックでは
/// この攻撃を防げない (= victim 側 actor 自身は自分の id を正しく返すため)。
///
/// **PR #78 review F-3 (case-insensitive lookup)**: `preferred_username` /
/// `host` の DB 列は `TEXT` で case-sensitive 比較になる。`@BOB@REMOTE.TEST`
/// のように mention を大文字で書かれても DB ヒットさせるため、lookup 時は
/// 両方を ASCII lowercase に倒す。`MentionAcct.name` (= 表示用) は元のケースを
/// 保持しているのでそちらに影響しない。
async fn resolve_mention_actor(state: &AppState, m: &MentionAcct) -> Result<ActorRow, String> {
    let user_lc = m.user.to_ascii_lowercase();
    let host_lc = m.host.to_ascii_lowercase();
    match repo::actor::get_by_username_host(state.pool(), &user_lc, &host_lc).await {
        Ok(Some(a)) => return Ok(a),
        Ok(None) => {}
        Err(err) => return Err(format!("mention {} DB lookup failed: {err}", m.name)),
    }
    if !state.enable_remote_fetch() {
        return Err(format!(
            "mention {} not found locally (remote fetch disabled in test mode)",
            m.name,
        ));
    }
    let acct = format!("{user_lc}@{host_lc}");
    let resolved = state
        .media_proxy()
        .resolve_webfinger(&acct)
        .await
        .map_err(|err| format_webfinger_err(&m.name, &err))?;
    // F-1: WebFinger が返した actor_uri のホストが、クエリしたホストと一致するか。
    // 不一致は cross-domain 差し替え攻撃の徴候なので reject。
    webfinger_guard::ensure_webfinger_host_match(&host_lc, &resolved.actor_uri)
        .map_err(|err| format!("mention {} {err}", m.name))?;
    remote_actor::fetch_and_upsert(state, &resolved.actor_uri)
        .await
        .map_err(|err| format_fetch_err(&m.name, &err))
}

fn format_webfinger_err(name: &str, err: &MediaProxyError) -> String {
    match err {
        MediaProxyError::Upstream {
            status,
            reason,
            message,
        } => format!(
            "mention {name} webfinger resolve failed (HTTP {status}, reason={reason}): {message}",
        ),
        other => format!("mention {name} webfinger resolve failed: {other}"),
    }
}

fn format_fetch_err(name: &str, err: &FetchError) -> String {
    match err {
        FetchError::Blocked { host, reason } => {
            format!("mention {name} actor fetch blocked: host {host:?} → {reason}")
        }
        FetchError::Malformed(msg) => format!("mention {name} actor malformed: {msg}"),
        other => format!("mention {name} actor fetch failed: {other}"),
    }
}

/// 添付メディア id を順序保ったまま `MediaRow` 配列に解決する。
///
/// - 重複 id は最初の出現だけ残す ── 同じ画像を 2 回貼る意味は無い。
/// - 件数上限 [`ATTACHMENT_MAX`] を超えたら 400。
/// - 各 id について「DB に存在する / 所有者一致 / `note_id IS NULL`」を確認。
///   `note_id` が既に埋まっていれば「他 Note の添付」なので拒否する。
async fn load_attachments(
    state: &AppState,
    local_actor: &ActorRow,
    ids: &[i64],
) -> Result<Vec<MediaRow>, Response> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > ATTACHMENT_MAX {
        return Err(bad_request("attachment_ids exceeds the 4-item limit"));
    }
    // 順序保ったまま dedupe。`Vec::contains` は O(n) だが、N <= 4 なので
    // HashSet を引かない方が小さく早い。
    let mut deduped: Vec<i64> = Vec::with_capacity(ids.len());
    for id in ids {
        if !deduped.contains(id) {
            deduped.push(*id);
        }
    }

    let rows = match repo::media::list_by_ids(state.pool(), &deduped).await {
        Ok(v) => v,
        Err(err) => {
            error!(?err, "POST /api/v1/notes: media lookup failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };
    // 配列 → id 引き map。順序復元のため一旦索引化する。
    let mut by_id: std::collections::HashMap<i64, MediaRow> =
        rows.into_iter().map(|m| (m.id, m)).collect();
    let mut ordered: Vec<MediaRow> = Vec::with_capacity(deduped.len());
    for id in &deduped {
        let Some(row) = by_id.remove(id) else {
            return Err(bad_request_owned(&format!(
                "attachment media id {id} not found"
            )));
        };
        if row.owner_actor_id != local_actor.id {
            warn!(
                media_id = id,
                owner = row.owner_actor_id,
                "POST /api/v1/notes: attachment owner mismatch"
            );
            return Err(error_with_body(
                StatusCode::FORBIDDEN,
                "attachment is not owned by the local actor",
            ));
        }
        if row.note_id.is_some() {
            return Err(bad_request_owned(&format!(
                "attachment media id {id} is already attached to another note"
            )));
        }
        ordered.push(row);
    }
    Ok(ordered)
}

/// 入力 → DB 行: tx で `insert` + `set_ap_id_and_url` + (M7) `attach_to_note`。
/// 成功時は `id`、失敗時はログだけ残して `Err(())` (上位は 503 で吸収)。
#[allow(clippy::too_many_arguments)]
async fn persist_note(
    state: &AppState,
    local_actor: &ActorRow,
    req: &CreateNoteRequest,
    prepared: &PreparedNote,
    visibility: Visibility,
    published_at: chrono::DateTime<chrono::Utc>,
    attachments: &[MediaRow],
    reply_parent: Option<&ReplyParentInfo>,
) -> Result<i64, ()> {
    let mut tx = match state.pool().begin().await {
        Ok(tx) => tx,
        Err(err) => {
            error!(?err, "POST /api/v1/notes: tx begin failed");
            return Err(());
        }
    };

    // reply 先 Note の note_id 紐付け。pool で事前に取れていれば (= `reply_parent`
    // が Some) それを使う。事前に取れていない場合のみ tx 内で再 lookup する ──
    // ローカル投稿の自己 reply は insert 直前に親が commit される race を
    // 救うため、tx 内検索を fallback として残す (PR #33 review #1 の意図)。
    let in_reply_to_note_id = if let Some(p) = reply_parent {
        Some(p.note_id)
    } else if let Some(ref reply_uri) = req.in_reply_to_ap_id {
        match repo::note::get_by_ap_id(&mut *tx, reply_uri).await {
            Ok(Some(row)) => Some(row.id),
            Ok(None) => None,
            Err(err) => {
                warn!(?err, %reply_uri, "in_reply_to_ap_id lookup failed; ignoring");
                None
            }
        }
    } else {
        None
    };

    let placeholder_ap_id = format!("urn:sakurasato:pending:{}", placeholder_token());
    let new_note = repo::note::NewNote {
        ap_id: placeholder_ap_id,
        actor_id: local_actor.id,
        content: req.content.clone(),
        language: req.language.clone(),
        in_reply_to_ap_id: req.in_reply_to_ap_id.clone(),
        in_reply_to_note_id,
        summary: prepared.summary.clone(),
        visibility,
        sensitive: prepared.sensitive,
        to_recipients: prepared.to.clone(),
        cc_recipients: prepared.cc.clone(),
        attachments: JsonValue::Array(prepared.attachment_documents.clone()),
        tags: JsonValue::Array(prepared.all_tags()),
        is_local: true,
        url: None,
        published_at,
    };

    let inserted = match repo::note::insert(&mut *tx, new_note).await {
        Ok(row) => row,
        Err(err) => {
            error!(?err, "POST /api/v1/notes: note insert failed");
            return Err(());
        }
    };

    let canonical_url = format!(
        "https://{host}/notes/{id}",
        host = state.config().server.host,
        id = inserted.id,
    );
    // 現状は `ap_id == url` が常に同値。将来、カスタムドメインや短縮 URL を
    // 導入したときに分岐できるよう、`set_ap_id_and_url` は 2 引数で受ける
    // 形にしてある (PR #33 review #2)。
    if let Err(err) =
        repo::note::set_ap_id_and_url(&mut *tx, inserted.id, &canonical_url, &canonical_url).await
    {
        error!(
            ?err,
            note_id = inserted.id,
            "POST /api/v1/notes: set_ap_id_and_url failed"
        );
        return Err(());
    }
    // M7: 添付メディア行を `note_id` でこの Note に紐付ける。`attach_to_note`
    // は所有者一致 + `note_id IS NULL` の行だけ更新するので、`load_attachments`
    // と二重に保護される (= 検査と更新の間に他リクエストが奪っても rows_affected
    // で検知できる)。`rows_affected != attachments.len()` なら誰かに横取り
    // されているので tx ロールバック扱いで失敗にする。
    if !attachments.is_empty() {
        let ids: Vec<i64> = attachments.iter().map(|m| m.id).collect();
        let updated =
            match repo::media::attach_to_note(&mut *tx, &ids, local_actor.id, inserted.id).await {
                Ok(n) => n,
                Err(err) => {
                    error!(?err, "POST /api/v1/notes: media attach_to_note failed");
                    return Err(());
                }
            };
        if usize::try_from(updated).unwrap_or(usize::MAX) != attachments.len() {
            warn!(
                expected = attachments.len(),
                actually_attached = updated,
                "POST /api/v1/notes: attachment race detected; aborting tx"
            );
            return Err(());
        }
    }
    if let Err(err) = tx.commit().await {
        error!(?err, "POST /api/v1/notes: tx commit failed");
        return Err(());
    }
    Ok(inserted.id)
}

/// 配送先 inbox を列挙し、`Create` を inbox ごとに 1 行ずつ enqueue する。
/// 戻り値は実際に積まれた件数 (= 成功した enqueue の合計)。
///
/// `extra_inboxes` は #64 で導入: 返信先 author の inbox など、followers 集合
/// に含まれない宛先を後付けで足す。**#65** で mention された remote actor の
/// inbox もここに混ぜる。followers と重複する inbox は dedupe で 1 回だけ
/// enqueue する (= `shared_inbox` を共有しているケースなど)。
///
/// `skip_followers = true` (= direct visibility) のときは followers 集合への
/// 配送を一切行わず、`extra_inboxes` だけを宛先にする。direct DM は mention
/// 先以外には配送してはならない、というのが Mastodon 互換 (#65)。
///
/// **#64 F-2 (fail-closed)**: `list_accepted_inboxes` が DB 障害で失敗した
/// 場合は `extra_inboxes` の配送も諦め、`queued_deliveries = 0` でレスポンス
/// する。フォロワー集合が分からないまま reply-parent だけ届ける「部分配送」
/// は、ユーザに「配送済み」と誤認させかねず、可視性スコープも崩す。失敗側に
/// 倒して 0 を返し、ユーザ側の再送 (= 同じ note を再 POST するか worker の
/// retry に任せる) で復旧する設計。`skip_followers = true` のときは followers
/// を引かないので、この fail-closed の経路を踏まない (= mention 配送は走る)。
async fn enqueue_deliveries(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
    extra_inboxes: &[String],
    skip_followers: bool,
) -> usize {
    let mut inboxes: Vec<String> = if skip_followers {
        Vec::new()
    } else {
        match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
            Ok(list) => list,
            Err(err) => {
                warn!(
                    ?err,
                    "POST /api/v1/notes: list_accepted_inboxes failed; skipping all deliveries (fail-closed)",
                );
                // fail-closed: extra_inboxes も含めて配送しない。
                return 0;
            }
        }
    };
    for extra in extra_inboxes {
        if !inboxes.iter().any(|i| i == extra) {
            inboxes.push(extra.clone());
        }
    }
    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_row) => queued += 1,
            Err(err) => {
                warn!(?err, %inbox, "POST /api/v1/notes: enqueue failed");
            }
        }
    }
    queued
}

fn validate_request(req: &CreateNoteRequest) -> Result<(), &'static str> {
    validate_content(&req.content)?;
    if let Some(ref s) = req.summary {
        validate_summary(s)?;
    }
    if let Some(ref reply) = req.in_reply_to_ap_id {
        validate_reply_url(reply)?;
    }
    Ok(())
}

#[derive(Debug)]
enum LocalActorError {
    Missing,
    Db(sqlx::Error),
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, LocalActorError> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(LocalActorError::Db)?;
    match row {
        Some(a) if a.is_local => Ok(a),
        _ => Err(LocalActorError::Missing),
    }
}

fn parse_visibility(s: Option<&str>) -> Result<Visibility, &'static str> {
    match s.unwrap_or("public") {
        "public" => Ok(Visibility::Public),
        "unlisted" => Ok(Visibility::Unlisted),
        "followers" => Ok(Visibility::Followers),
        "direct" => Ok(Visibility::Direct),
        _ => Err("invalid visibility: must be one of public/unlisted/followers/direct"),
    }
}

fn validate_content(content: &str) -> Result<(), &'static str> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("content must not be empty");
    }
    if content.chars().count() > CONTENT_MAX {
        return Err("content exceeds the 5000-character limit");
    }
    Ok(())
}

fn validate_summary(summary: &str) -> Result<(), &'static str> {
    if summary.chars().count() > SUMMARY_MAX {
        return Err("summary exceeds the 200-character limit");
    }
    Ok(())
}

fn validate_reply_url(uri: &str) -> Result<(), &'static str> {
    let url = url::Url::parse(uri).map_err(|_| "in_reply_to_ap_id is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("in_reply_to_ap_id scheme must be http or https");
    }
    if url.host_str().is_none() {
        return Err("in_reply_to_ap_id must have a host");
    }
    // **#64 F-3**: private / loopback / link-local / reserved IP の URI を弾く。
    // server は in_reply_to_ap_id を fetch しない (= SSRF は起きない) が、
    // `Create` activity の `inReplyTo` としてそのまま federate されるため、
    // 内部 topology の漏洩を防ぐ。defense-in-depth として配送系と同じガード
    // (= [`crate::net_guard::host_blocked`]) を通す。
    if crate::net_guard::host_blocked(&url).is_some() {
        return Err("in_reply_to_ap_id host is in a blocked address range");
    }
    Ok(())
}

/// visibility → AS2 to/cc を組み立てる。CLAUDE.md の M4 PR2 仕様準拠 +
/// **#65** で direct を実装した版:
///
/// - public:    to = [Public]            cc = [followers, mentions..., `reply_parent`?]
/// - unlisted:  to = [followers]         cc = [Public, mentions..., `reply_parent`?]
/// - followers: to = [followers]         cc = [mentions..., `reply_parent`?]
/// - direct:    to = [mentions..., `reply_parent`?]   cc = []
///
/// **#64**: `reply_parent_uri` が `Some` なら、direct は `to` に、それ以外は
/// `cc` に追加する。既存要素と重複する場合は足さない。
///
/// **#65**: `mention_uris` は解決済み mention の actor URI。direct は
/// `to`、それ以外は `cc` に積む。重複は dedupe。
fn recipients_for(
    v: Visibility,
    followers_url: &str,
    reply_parent_uri: Option<&str>,
    mention_uris: &[&str],
) -> (Vec<String>, Vec<String>) {
    let (mut to, mut cc) = match v {
        Visibility::Public => (vec![PUBLIC_URI.into()], vec![followers_url.into()]),
        Visibility::Unlisted => (vec![followers_url.into()], vec![PUBLIC_URI.into()]),
        Visibility::Followers => (vec![followers_url.into()], vec![]),
        Visibility::Direct => (vec![], vec![]),
    };
    let target = if matches!(v, Visibility::Direct) {
        &mut to
    } else {
        &mut cc
    };
    for uri in mention_uris {
        if !target.iter().any(|s| s == uri) {
            target.push((*uri).to_string());
        }
    }
    if let Some(uri) = reply_parent_uri
        && !target.iter().any(|s| s == uri)
    {
        target.push(uri.to_string());
    }
    (to, cc)
}

#[allow(clippy::too_many_arguments)]
fn build_create_activity(
    actor: &ActorRow,
    note_url: &str,
    content: &str,
    summary: Option<&str>,
    sensitive: bool,
    language: Option<&str>,
    in_reply_to: Option<&str>,
    to: &[String],
    cc: &[String],
    attachments: &[JsonValue],
    tag: &[JsonValue],
    published_at: chrono::DateTime<chrono::Utc>,
) -> JsonValue {
    let published = published_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let activity_id = format!("{note_url}/activity");

    let mut note = json!({
        "type": "Note",
        "id": note_url,
        "attributedTo": actor.ap_id,
        "content": content,
        "to": to,
        "cc": cc,
        "published": published,
        "sensitive": sensitive,
        "url": note_url,
    });
    if let Some(s) = summary {
        note["summary"] = JsonValue::String(s.into());
    }
    if let Some(lang) = language {
        note["contentMap"] = json!({ lang: content });
    }
    if let Some(reply) = in_reply_to {
        note["inReplyTo"] = JsonValue::String(reply.into());
    }
    if !attachments.is_empty() {
        note["attachment"] = JsonValue::Array(attachments.to_vec());
    }
    if !tag.is_empty() {
        note["tag"] = JsonValue::Array(tag.to_vec());
    }

    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Create",
        "id": activity_id,
        "actor": actor.ap_id,
        "to": to,
        "cc": cc,
        "published": published,
        "object": note,
    })
}

fn bad_request(reason: &'static str) -> Response {
    error_with_body(StatusCode::BAD_REQUEST, reason)
}

fn bad_request_owned(reason: &str) -> Response {
    error_with_body(StatusCode::BAD_REQUEST, reason)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

/// 衝突しにくい placeholder 文字列を作る。tx 内で書き直すまでの一瞬しか
/// 残らないが、UNIQUE 制約に衝突する確率を抑えたいので時刻 + ナノ秒を載せる。
/// `uuid` クレートを足したくないので chrono の単調系で代用 (お一人様サーバ
/// の同時投稿は事実上 1 件のみ)。
fn placeholder_token() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}-{}",
        now.timestamp_nanos_opt().unwrap_or(now.timestamp_micros()),
        std::process::id(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_visibility_accepts_known_values() {
        assert_eq!(parse_visibility(None), Ok(Visibility::Public));
        assert_eq!(parse_visibility(Some("public")), Ok(Visibility::Public));
        assert_eq!(parse_visibility(Some("unlisted")), Ok(Visibility::Unlisted));
        assert_eq!(
            parse_visibility(Some("followers")),
            Ok(Visibility::Followers)
        );
        // **#65**: direct も受理する。
        assert_eq!(parse_visibility(Some("direct")), Ok(Visibility::Direct));
        assert!(parse_visibility(Some("private")).is_err());
    }

    #[test]
    fn validate_content_rejects_empty_and_too_long() {
        assert!(validate_content("").is_err());
        assert!(validate_content("   ").is_err());
        assert!(validate_content("hello").is_ok());
        let too_long = "あ".repeat(CONTENT_MAX + 1);
        assert!(validate_content(&too_long).is_err());
        let limit = "あ".repeat(CONTENT_MAX);
        assert!(validate_content(&limit).is_ok());
    }

    #[test]
    fn validate_summary_caps_length() {
        assert!(validate_summary("").is_ok());
        let max = "x".repeat(SUMMARY_MAX);
        assert!(validate_summary(&max).is_ok());
        let over = "x".repeat(SUMMARY_MAX + 1);
        assert!(validate_summary(&over).is_err());
    }

    #[test]
    fn validate_reply_url_filters_schemes() {
        assert!(validate_reply_url("https://example.com/notes/1").is_ok());
        assert!(validate_reply_url("http://example.com/notes/1").is_ok());
        assert!(validate_reply_url("file:///etc/passwd").is_err());
        assert!(validate_reply_url("not a url").is_err());
        assert!(validate_reply_url("https:///").is_err());
    }

    /// **#64 F-3**: private / loopback / link-local の IP literal を弾く。
    /// `inReplyTo` フィールドとして federate される文字列なので、内部
    /// topology を漏らさないよう `net_guard` を通す。
    #[test]
    fn validate_reply_url_blocks_private_address_ranges() {
        assert!(validate_reply_url("http://127.0.0.1/notes/1").is_err());
        assert!(validate_reply_url("http://192.168.1.1/notes/1").is_err());
        assert!(validate_reply_url("http://10.0.0.1/notes/1").is_err());
        assert!(validate_reply_url("http://169.254.169.254/notes/1").is_err());
        assert!(validate_reply_url("http://localhost/notes/1").is_err());
    }

    #[test]
    fn recipients_match_visibility_spec() {
        let f = "https://x.test/users/alice/followers";
        assert_eq!(
            recipients_for(Visibility::Public, f, None, &[]),
            (vec![PUBLIC_URI.into()], vec![f.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Unlisted, f, None, &[]),
            (vec![f.into()], vec![PUBLIC_URI.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Followers, f, None, &[]),
            (vec![f.into()], vec![]),
        );
    }

    /// **#64**: 返信時、parent author URI が `cc` に乗ること (public/unlisted/
    /// followers の 3 visibility で確認)。
    #[test]
    fn recipients_append_reply_parent_to_cc() {
        let f = "https://x.test/users/alice/followers";
        let parent = "https://remote.test/users/bob";
        assert_eq!(
            recipients_for(Visibility::Public, f, Some(parent), &[]),
            (vec![PUBLIC_URI.into()], vec![f.into(), parent.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Unlisted, f, Some(parent), &[]),
            (vec![f.into()], vec![PUBLIC_URI.into(), parent.into()],),
        );
        assert_eq!(
            recipients_for(Visibility::Followers, f, Some(parent), &[]),
            (vec![f.into()], vec![parent.into()]),
        );
    }

    /// **#64**: direct visibility のときは `to` に親 author を載せる。
    #[test]
    fn recipients_append_reply_parent_to_to_when_direct() {
        let f = "https://x.test/users/alice/followers";
        let parent = "https://remote.test/users/bob";
        assert_eq!(
            recipients_for(Visibility::Direct, f, Some(parent), &[]),
            (vec![parent.into()], vec![]),
        );
    }

    /// **#64**: 既存要素と重複する URI は二重には載せない。
    #[test]
    fn recipients_dedupe_reply_parent_against_existing_cc() {
        let f = "https://x.test/users/alice/followers";
        // 既存 cc にある followers URL を parent として渡しても 1 件のまま。
        assert_eq!(
            recipients_for(Visibility::Public, f, Some(f), &[]),
            (vec![PUBLIC_URI.into()], vec![f.into()]),
        );
    }

    /// **#65**: mention URI は visibility に応じて to (direct) / cc (それ以外)
    /// に乗る。
    #[test]
    fn recipients_append_mentions_per_visibility() {
        let f = "https://x.test/users/alice/followers";
        let bob = "https://remote.test/users/bob";
        let carol = "https://other.test/users/carol";
        assert_eq!(
            recipients_for(Visibility::Public, f, None, &[bob, carol]),
            (
                vec![PUBLIC_URI.into()],
                vec![f.into(), bob.into(), carol.into()]
            ),
        );
        assert_eq!(
            recipients_for(Visibility::Unlisted, f, None, &[bob]),
            (vec![f.into()], vec![PUBLIC_URI.into(), bob.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Followers, f, None, &[bob]),
            (vec![f.into()], vec![bob.into()]),
        );
        // direct: mentions が `to` に並ぶ。followers / Public は乗らない。
        assert_eq!(
            recipients_for(Visibility::Direct, f, None, &[bob, carol]),
            (vec![bob.into(), carol.into()], vec![]),
        );
    }

    /// **#65**: 同じ URI を mention + `reply_parent` で受けても 1 件のみ載る。
    #[test]
    fn recipients_dedupe_mention_and_reply_parent() {
        let f = "https://x.test/users/alice/followers";
        let bob = "https://remote.test/users/bob";
        assert_eq!(
            recipients_for(Visibility::Direct, f, Some(bob), &[bob]),
            (vec![bob.into()], vec![]),
        );
        assert_eq!(
            recipients_for(Visibility::Public, f, Some(bob), &[bob]),
            (vec![PUBLIC_URI.into()], vec![f.into(), bob.into()]),
        );
    }

    /// **#65**: ASCII の `@user@host` を抽出する。先頭・空白後・記号後は OK、
    /// 直前が単語境界でない (= メアド) はスキップ。末尾ピリオドは host から
    /// 削る。
    #[test]
    fn parse_mentions_basic_cases() {
        let m = parse_mentions("hi @bob@example.com and @carol@b.example!");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].user, "bob");
        assert_eq!(m[0].host, "example.com");
        assert_eq!(m[0].name, "@bob@example.com");
        assert_eq!(m[1].user, "carol");
        assert_eq!(m[1].host, "b.example");
    }

    #[test]
    fn parse_mentions_skips_email_like() {
        // 前が単語 (`d`) なので mention 開始と認識しない。
        let m = parse_mentions("send email to bob@example.com please");
        assert!(m.is_empty(), "expected no mentions, got {m:?}");
    }

    // ── parse_emoji_shortcodes (Issue #102) ─────────────────────

    #[test]
    fn parse_emoji_basic() {
        let v = parse_emoji_shortcodes("hello :sakura: world");
        assert_eq!(v, vec!["sakura"]);
    }

    #[test]
    fn parse_emoji_multiple_and_dedupe() {
        let v = parse_emoji_shortcodes(":a: :b: :a: :c:");
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_emoji_ignores_invalid_chars() {
        // 内部にスペース / `@` / `.` がある shortcode は無視 (= 終端 `:` が
        // 来ないため打ち切り)。
        let v = parse_emoji_shortcodes(":foo bar: :baz.qux: :alice@host:");
        assert!(v.is_empty(), "got {v:?}");
    }

    #[test]
    fn parse_emoji_lowercase_dedupe() {
        let v = parse_emoji_shortcodes(":Sakura: :SAKURA: :sakura:");
        assert_eq!(v, vec!["sakura"]);
    }

    #[test]
    fn parse_emoji_underscore_hyphen_digits_ok() {
        let v = parse_emoji_shortcodes(":hello-1: :foo_bar: :u_2:");
        assert_eq!(v, vec!["hello-1", "foo_bar", "u_2"]);
    }

    #[test]
    fn parse_emoji_skips_too_long_shortcode() {
        // Issue #188: 上限を 64 → 128 に緩和。128 chars はギリギリ拾い、
        // 129 chars は drop する境界回帰テスト。
        let exact_128 = "a".repeat(128);
        let v = parse_emoji_shortcodes(&format!(":{exact_128}:"));
        assert_eq!(v, vec![exact_128.clone()], "128 chars should be accepted");

        let too_long = "a".repeat(129);
        let v = parse_emoji_shortcodes(&format!(":{too_long}:"));
        assert!(v.is_empty(), "129 chars should be dropped; got {v:?}");

        // 旧上限 (= 65 chars) は受理されるように。これが本 issue の主旨。
        let medium = "a".repeat(65);
        let v = parse_emoji_shortcodes(&format!(":{medium}:"));
        assert_eq!(
            v,
            vec![medium],
            "65 chars should be accepted (was rejected pre-#188)"
        );
    }

    #[test]
    fn parse_emoji_after_japanese_works() {
        let v = parse_emoji_shortcodes("こんにちは:sakura:");
        assert_eq!(v, vec!["sakura"]);
    }

    #[test]
    fn parse_emoji_respects_max_limit() {
        use std::fmt::Write as _;
        let mut s = String::new();
        for n in 0..200 {
            let _ = write!(s, ":e{n}: ");
        }
        let v = parse_emoji_shortcodes(&s);
        assert_eq!(v.len(), EMOJI_MAX);
    }

    #[test]
    fn parse_mentions_trims_trailing_dot() {
        let m = parse_mentions("ping @bob@example.com.");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].host, "example.com");
    }

    #[test]
    fn parse_mentions_requires_dot_in_host() {
        let m = parse_mentions("hi @bob@localhost");
        // host に `.` が無いので mention 扱いしない (= 連合相手にならない)。
        assert!(m.is_empty(), "expected no mentions, got {m:?}");
    }

    #[test]
    fn parse_mentions_after_japanese_works() {
        // マルチバイト UTF-8 の直後でも prev_ok が true になり拾える。
        let m = parse_mentions("こんにちは@bob@example.com");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "@bob@example.com");
    }

    #[test]
    fn parse_mentions_dedupes_case_insensitive_repeats() {
        let m = parse_mentions("@Bob@Example.com hi @bob@example.com");
        assert_eq!(m.len(), 1);
        // 最初に出てきた形をそのまま保持する。
        assert_eq!(m[0].name, "@Bob@Example.com");
    }

    #[test]
    fn parse_mentions_handles_parentheses() {
        let m = parse_mentions("see (@alice@x.test) for details");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "@alice@x.test");
    }

    /// **PR #78 review F-2**: 空白なしで連続する `@user@host@user@host` でも
    /// 2 件目を黙って捨てない (= 前回 mention 末尾を boundary と認める)。
    #[test]
    fn parse_mentions_handles_adjacent_pair_without_whitespace() {
        let m = parse_mentions("@alice@a.test@bob@b.test");
        assert_eq!(m.len(), 2, "expected 2 mentions, got {m:?}");
        assert_eq!(m[0].name, "@alice@a.test");
        assert_eq!(m[1].name, "@bob@b.test");
    }

    /// **PR #78 review F-2**: 直後に通常文字が来た場合は flag が解除されて
    /// 次の `@` は通常の boundary 判定に戻る (= メアド誤認の回避は維持)。
    #[test]
    fn parse_mentions_boundary_flag_resets_after_nonat_char() {
        // mention 直後にスペース、その後にメアド風 (= `text@host`) があっても
        // 拾わないこと。
        let m = parse_mentions("@alice@a.test bob@b.test");
        assert_eq!(m.len(), 1, "expected only alice, got {m:?}");
        assert_eq!(m[0].name, "@alice@a.test");
    }

    // `ensure_webfinger_host_match` の挙動テストは
    // [`crate::webfinger_guard::tests`] に集約済 (PR1 / Issue #79 で共通化)。

    /// **#65 (review #2)**: 同一投稿で `MENTION_MAX` を超える mention は
    /// 配送経路の `DoS` 防止に巻き込まれ得るため、`MENTION_MAX` の値が
    /// 妥当 (= Mastodon 慣習に近い 50) で固定されていることを assert する。
    /// 上限超過時の 400 は統合テスト経路でカバー (= 50 件ちょうどは通り、
    /// 51 件は弾かれる) ── unit test 段ではコンスタント値の固定のみ確認。
    #[test]
    fn mention_max_is_pinned_to_fifty() {
        assert_eq!(MENTION_MAX, 50);
    }
}
