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
//!   PR2 では `direct` は未対応 (宛先 actor を解決する経路が無いため 400)。
//! - `in_reply_to_ap_id`: 任意。`url::Url::parse` で `http`/`https` のみ
//!   受け入れる。host 必須。**#64**: 返信先 Note を DB から引き、その
//!   `attributedTo` actor を `cc` (direct なら `to`) に追加し、remote なら
//!   親 author の inbox も `delivery_queue` に積む ── 未フォロー相手への
//!   返信が連合相手から「気付かれない」既知バグの解消。
//!
//! ## エラー
//!
//! - 400: バリデーション失敗 (content 空 / 長すぎ / visibility 不正 等)
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
use crate::local_api::media::build_media_url;
use crate::local_api::stream::{NoteCreatedPayload, TimelineEvent};
use crate::state::AppState;

const CONTENT_MAX: usize = 5_000;
const SUMMARY_MAX: usize = 200;
const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";
/// 添付の最大件数。Mastodon API の 4 件と揃える ── 連合相手にも違和感が
/// 出ない値で、お一人様サーバとしても十分。
const ATTACHMENT_MAX: usize = 4;

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

    let published_at = Utc::now();
    let prepared = PreparedNote::from_request(
        &req,
        &local_actor,
        visibility,
        &attachments,
        reply_parent.as_ref(),
        &state,
    );

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
        published_at,
    );
    let extra_inboxes: Vec<String> = reply_parent
        .as_ref()
        .and_then(|p| p.inbox_for_delivery.clone())
        .into_iter()
        .collect();
    let queued = enqueue_deliveries(&state, &local_actor, &activity, &extra_inboxes).await;

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
}

impl PreparedNote {
    fn from_request(
        req: &CreateNoteRequest,
        actor: &ActorRow,
        visibility: Visibility,
        attachments: &[MediaRow],
        reply_parent: Option<&ReplyParentInfo>,
        state: &AppState,
    ) -> Self {
        let followers_url = actor
            .followers_url
            .clone()
            .unwrap_or_else(|| format!("{}/followers", actor.ap_id));
        let (to, cc) = recipients_for(
            visibility,
            &followers_url,
            reply_parent.map(|p| p.actor_uri.as_str()),
        );
        let host = &state.config().server.host;
        let attachment_documents = attachments
            .iter()
            .map(|m| attachment_document(host, m))
            .collect();
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
        }
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
    Some(ReplyParentInfo {
        note_id: parent_note.id,
        actor_uri: parent_actor.ap_id,
        inbox_for_delivery,
    })
}

/// 1 件の `media` 行を AP の `Document` JSON にする。
///
/// AS2 `Document` で `mediaType` + `url` + `name` (alt) を載せる。`width` /
/// `height` は Mastodon 拡張だが幅広く受け入れられている (Misskey も読む)。
fn attachment_document(host: &str, m: &MediaRow) -> JsonValue {
    let mut obj = json!({
        "type": "Document",
        "mediaType": m.media_type,
        "url": build_media_url(host, &m.storage_key),
        "width": m.width,
        "height": m.height,
    });
    if let Some(alt) = m.alt_text.as_ref()
        && !alt.is_empty()
    {
        obj["name"] = JsonValue::String(alt.clone());
    }
    obj
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
        tags: JsonValue::Array(vec![]),
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
/// に含まれない宛先を後付けで足す。followers と重複する inbox は dedupe で
/// 1 回だけ enqueue する (= `shared_inbox` を共有しているケースなど)。
///
/// **#64 F-2 (fail-closed)**: `list_accepted_inboxes` が DB 障害で失敗した
/// 場合は `extra_inboxes` の配送も諦め、`queued_deliveries = 0` でレスポンス
/// する。フォロワー集合が分からないまま reply-parent だけ届ける「部分配送」
/// は、ユーザに「配送済み」と誤認させかねず、可視性スコープも崩す。失敗側に
/// 倒して 0 を返し、ユーザ側の再送 (= 同じ note を再 POST するか worker の
/// retry に任せる) で復旧する設計。
async fn enqueue_deliveries(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
    extra_inboxes: &[String],
) -> usize {
    let mut inboxes = match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await
    {
        Ok(list) => list,
        Err(err) => {
            warn!(
                ?err,
                "POST /api/v1/notes: list_accepted_inboxes failed; skipping all deliveries (fail-closed)",
            );
            // fail-closed: extra_inboxes も含めて配送しない。
            return 0;
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
        "direct" => Err("direct visibility is not supported yet"),
        _ => Err("invalid visibility: must be one of public/unlisted/followers"),
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

/// visibility → AS2 to/cc を組み立てる。CLAUDE.md の M4 PR2 仕様準拠:
///
/// - public:    to = [Public]            cc = [followers]
/// - unlisted:  to = [followers]         cc = [Public]
/// - followers: to = [followers]         cc = []
/// - direct:    本 PR では到達しない (`parse_visibility` で reject 済み)
///
/// **#64**: `reply_parent_uri` が `Some` なら、direct は `to` に、それ以外は
/// `cc` に追加する。既存要素と重複する場合は足さない。
fn recipients_for(
    v: Visibility,
    followers_url: &str,
    reply_parent_uri: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let (mut to, mut cc) = match v {
        Visibility::Public => (vec![PUBLIC_URI.into()], vec![followers_url.into()]),
        Visibility::Unlisted => (vec![followers_url.into()], vec![PUBLIC_URI.into()]),
        Visibility::Followers => (vec![followers_url.into()], vec![]),
        // direct は PR2 では弾く想定だが、network of trust として match 漏れを
        // 起こさないため to=[] / cc=[] でフェイルセーフ返却。
        Visibility::Direct => (vec![], vec![]),
    };
    if let Some(uri) = reply_parent_uri {
        let bucket = if matches!(v, Visibility::Direct) {
            &mut to
        } else {
            &mut cc
        };
        if !bucket.iter().any(|s| s == uri) {
            bucket.push(uri.to_string());
        }
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
        assert!(parse_visibility(Some("direct")).is_err());
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
            recipients_for(Visibility::Public, f, None),
            (vec![PUBLIC_URI.into()], vec![f.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Unlisted, f, None),
            (vec![f.into()], vec![PUBLIC_URI.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Followers, f, None),
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
            recipients_for(Visibility::Public, f, Some(parent)),
            (vec![PUBLIC_URI.into()], vec![f.into(), parent.into()]),
        );
        assert_eq!(
            recipients_for(Visibility::Unlisted, f, Some(parent)),
            (vec![f.into()], vec![PUBLIC_URI.into(), parent.into()],),
        );
        assert_eq!(
            recipients_for(Visibility::Followers, f, Some(parent)),
            (vec![f.into()], vec![parent.into()]),
        );
    }

    /// **#64**: direct visibility のときは `to` に親 author を載せる。
    #[test]
    fn recipients_append_reply_parent_to_to_when_direct() {
        let f = "https://x.test/users/alice/followers";
        let parent = "https://remote.test/users/bob";
        assert_eq!(
            recipients_for(Visibility::Direct, f, Some(parent)),
            (vec![parent.into()], vec![]),
        );
    }

    /// **#64**: 既存要素と重複する URI は二重には載せない。
    #[test]
    fn recipients_dedupe_reply_parent_against_existing_cc() {
        let f = "https://x.test/users/alice/followers";
        // 既存 cc にある followers URL を parent として渡しても 1 件のまま。
        assert_eq!(
            recipients_for(Visibility::Public, f, Some(f)),
            (vec![PUBLIC_URI.into()], vec![f.into()]),
        );
    }
}
