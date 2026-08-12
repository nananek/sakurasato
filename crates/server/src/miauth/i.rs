//! `POST /api/i` ── Misskey 互換 whoami (= M14 #158, 親 issue #150)。
//!
//! 認証された `MiAuth` token に紐付く ── お一人様サーバなら local actor 1 件 ──
//! を `MissUser` JSON で返す。Misskey クライアントは login flow の最初に
//! 必ずここを叩いて自身のユーザ情報をキャッシュする (= Milktea も `MissRirica`
//! も初期化時に確実に呼ぶ)。
//!
//! ## 認証経路
//!
//! Misskey 公式仕様では `POST /api/i { i: <token> }` で body 内に token を
//! 載せる。Sakurasato は **2 経路** をサポート (詳細は [`crate::miauth::auth`]):
//!
//! 1. **body `i` フィールド** (= 主流。Milktea / `MissRirica` / `MiPA`)
//! 2. **`Authorization: Bearer <token>` ヘッダ** (= Iceshrimp / Sharkey 流儀、
//!    既存 [`crate::local_api`] の Bearer 利用者からの cross-test を容易に)
//!
//! 両方が同時に存在するときは **body の `i` を優先** ── Misskey 公式仕様に
//! 倣う (= Bearer は互換用の代替)。`i` が無いとき Authorization ヘッダを試す。
//!
//! ## scope 要求
//!
//! `/api/i` は Misskey 仕様で **`read:account` scope を要求** する。token に
//! 該当 scope が無い場合は `403 PERMISSION_DENIED`。
//!
//! ## レスポンス
//!
//! [`crate::miauth::conv::MissUser`] (= minimum subset)。詳細フィールド (=
//! `createdAt` / `description` / `emojis` / `bannerUrl` 等) は #159 で
//! `UserDetailed` 形を追加するとき同タイミングで揃える。
//!
//! ## `POST /api/i/update` (プロフィール編集)
//!
//! Aria (`misskey_dart` `INotifier`) の `setName` / `setDescription` /
//! `setIsLocked` 等はすべてこの endpoint に集約される (Misskey wire 仕様)。
//! 未実装だと `ApiService.post` が 404 を投げ、`INotifier.setDescription` の
//! 呼び出し元 (プロフィール編集画面) が crash する ── `notes/mentions` /
//! `following/requests/*` と同種の「未実装 404 → crash」系バグ。詳細は
//! [`update`] のドキュメントを参照。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::{ActorField, ActorRow};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::local_api::media::build_media_url;
use crate::local_api::profile::{
    DISPLAY_NAME_MAX, SUMMARY_MAX, build_update_activity, enqueue_to_followers,
};
use crate::miauth::auth;
use crate::miauth::conv::{from_actor_me_detailed, resolve_user_emojis};
use crate::miauth::error::{bad_request, error_resp, internal_error};
use crate::miauth::meta::{build_policies, max_file_size_mb_from_bytes};
use crate::miauth::notes::resolve_self_actor;
use crate::state::AppState;

/// `/api/i` の `read:account` scope (= Misskey 仕様の hardcoded constant)。
/// scope 文字列は `crate::miauth::auth::has_scope` で完全一致判定される。
const SCOPE_READ_ACCOUNT: &str = "read:account";
/// `/api/i/update` の `write:account` scope (Misskey 本家準拠)。
/// [`crate::miauth::lists`] の `users/lists/*` 書き込み系と同じ scope 文字列。
const SCOPE_WRITE_ACCOUNT: &str = "write:account";

/// `location` / `lang` の文字数上限。Misskey 本家に明示的なプロトコル上限は
/// 無いが、無検証で無制限に受け付けると DB 肥大の入口になるので Sakurasato
/// 独自に妥当な上限を設ける。
const LOCATION_MAX: usize = 100;
const LANG_MAX: usize = 32;
/// `followedMessage` の文字数上限。
const FOLLOWED_MESSAGE_MAX: usize = 1000;
/// `fields` の最大件数。Misskey 本家の設定 UI も 4 件までしか登録できない。
const FIELDS_MAX_COUNT: usize = 4;
/// `fields` 各要素 (`name`/`value`) の文字数上限。
const FIELD_NAME_MAX: usize = 100;
const FIELD_VALUE_MAX: usize = 100;

/// `POST /api/i` の body。Misskey 公式は **`i` フィールドだけ** を持つ。
/// `serde(default)` で **`{}` body** や Bearer ヘッダ単独でも parse できる
/// (= `i` 無しでも 400 にならず、handler 内で Authorization 経路に倒す)。
#[derive(Debug, Deserialize, Default)]
pub struct IBody {
    #[serde(default)]
    pub i: Option<String>,
}

/// `POST /api/i` handler。
///
/// 認証フロー: body `i` → Authorization Bearer の順で raw token を探し、
/// [`auth::validate_token_raw`] で DB lookup する。見つからない / scope 不足
/// なら 401 / 403。成功すれば local actor の `MissUser` を返す。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<IBody>>,
) -> Response {
    // 1. raw token を取り出す。body 経由 → Authorization 経由の順で試す。
    //    body 経由は `Json<IBody>` を `Option` で受けることで「body 無し」
    //    (= Content-Length: 0、Bearer のみ) にも対応する。
    let raw = match body.as_ref().and_then(|j| j.i.as_deref()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(auth::parse_bearer_header)
        {
            Some(t) => t.to_string(),
            None => {
                return auth::unauthorized(
                    "missing token (provide body `i` or Authorization: Bearer)",
                );
            }
        },
    };

    // 2. token を DB lookup。失敗 (= unknown / revoked) は 401。
    let Some(token_row) = auth::validate_token_raw(&state, &raw).await else {
        return auth::unauthorized("invalid or revoked token");
    };

    // 3. scope 検査。`read:account` 必須。
    if !auth::has_scope(&token_row, SCOPE_READ_ACCOUNT) {
        return auth::forbidden(&format!("missing scope: {SCOPE_READ_ACCOUNT}"));
    }

    // 4. `last_used_at` を best-effort 更新 (= block しない)。
    auth::mark_used_async(&state, token_row.id);

    // 5. local actor + 集計 count → MeDetailed (M14 #170 で MissUser → MeDetailed
    //    に拡張、Aria / Milktea の self profile 描画用)。
    let me = match build_self_me_detailed(&state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    Json(me).into_response()
}

/// `/api/i` 用の `MeDetailed` JSON 構築 (= M14 #170)。
///
/// `MissUser` (= `UserLite` 最小) では Aria / Milktea 等が self profile を
/// 描画できないため、`MeDetailed` 相当に膨らませる。`UserDetailed` 部分は
/// `users/show` と同じ、Me 専用フィールドは [`from_actor_me_detailed`] で
/// 上書きする。`policies` は `/api/meta` と完全に同じ object を渡す ──
/// `media_proxy.max_bytes` を MiB に丸めて [`build_policies`] に渡せば
/// `maxFileSizeMb` 等が一致する。
///
/// `POST /api/miauth/{uuid}/check` ([`crate::miauth::check`]) の `user` フィールド
/// も **同じ self `MeDetailed`** を返す ── Aria (`misskey_dart`) は check レスポンス
/// の `user` を login 直後の self user として `MeDetailed` で parse し、`isBot` /
/// `isCat` 等の **required bool** を cast する。最小 `MissUser` (= `UserLite`) を
/// 返すと欠落フィールドが `null as bool` になり `type 'Null' is not a subtype of
/// type 'bool'` で crash するため、`/api/i` と `check` で同じ builder を共有する。
pub(crate) async fn build_self_me_detailed(state: &AppState) -> Result<JsonValue, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let actor = match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => {
            tracing::error!(host, user, "local actor not found for /api/i");
            return Err(crate::miauth::error::internal_error(
                "local actor not initialized; run `sakurasato init`",
            ));
        }
        Err(err) => {
            tracing::error!(?err, "local actor lookup failed for /api/i");
            return Err(crate::miauth::error::internal_error(
                "failed to look up local actor",
            ));
        }
    };
    let followers = repo::follow::count_followers(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    let following = repo::follow::count_following(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    let notes = repo::note::count_local(state.pool()).await.unwrap_or(0);

    // `/api/meta.policies` と同じ shape を再利用 ── `media_proxy.max_bytes`
    // → MiB の丸めも `meta::handle` と **共有 helper** で揃える ([PR #171
    // round-2 finding 3] 同じ式で両 endpoint の policies.maxFileSizeMb が
    // 乖離しないことを型レベルで保証)。
    let cfg = state.config();
    let max_file_size_mb = max_file_size_mb_from_bytes(cfg.media_proxy.max_bytes);
    let policies = build_policies(max_file_size_mb);

    // #206 PR2: in-app 通知の未読数。Aria 等の通知バッジ用。
    let unread = repo::notification::count_unread(state.pool(), actor.id)
        .await
        .unwrap_or(0);

    // 自分の display name / description / fields に埋め込んだ `:shortcode:` を
    // 解決して `emojis` map に載せる (= バグ 1: Aria 等が名前の絵文字を画像化
    // できるようにする)。解決できない shortcode は fail-open で空 map になる。
    let emojis = resolve_user_emojis(state.pool(), host, &actor).await;

    let mut me = from_actor_me_detailed(&actor, followers, following, notes, policies, emojis);
    if let Some(map) = me.as_object_mut() {
        map.insert(
            "unreadNotificationsCount".to_string(),
            serde_json::json!(unread),
        );
        map.insert(
            "hasUnreadNotification".to_string(),
            serde_json::json!(unread > 0),
        );
    }
    Ok(me)
}

/// `POST /api/i/update` body。Misskey 本家は 40 以上のプロフィール/設定項目を
/// 持つが、型を固定すると「未対応フィールドが 1 つでも来たら 400」になり
/// クライアントの設定画面 (= 一括 PATCH で全項目を送る) が丸ごと壊れる。
/// [`serde_json::Value`] で受けて、Sakurasato が実際に列を持つフィールドだけ
/// [`update`] 内で個別に取り出す (= `crate::miauth::drive::update` の
/// `comment`/`name`/`isSensitive` と同じ「受理するが対応列のみ反映」方針)。
///
/// `write:account` を要求する。
///
/// ## 反映されるフィールド
///
/// | wire (Misskey)    | 内部列                                | 備考                       |
/// |--------------------|----------------------------------------|----------------------------|
/// | `name`             | `actor.display_name`                   | 100 文字上限               |
/// | `description`      | `actor.summary`                        | 5000 文字上限              |
/// | `avatarId`         | `actor.icon_url`                       | 所有 drive file のみ許可   |
/// | `bannerId`         | `actor.image_url`                      | 同上                       |
/// | `isLocked`         | `actor.manually_approves_followers`    | `actor lock`/`unlock` と同じ列 |
/// | `birthday`         | `actor.birthday`                       | `"YYYY-MM-DD"` ISO 日付文字列 |
/// | `location`         | `actor.location`                       | 自由記述、検証無し          |
/// | `lang`             | `actor.lang`                           | 自由記述、検証無し          |
/// | `followedMessage`  | `actor.followed_message`               | 1000 文字上限               |
/// | `fields`           | `actor.fields` (JSONB 配列)             | 最大 4 件、name/value 各 100 文字上限 |
///
/// 上記以外 (`isBot` / `isExplorable` / `mutedWords` /
/// `notificationRecieveConfig` 等) は **受理するが値を捨てる**。Sakurasato は
/// これらのバックエンド列を持たず、`/api/i` でも固定値を返している
/// ([`from_actor_me_detailed`] 参照) ── 実装するときは読み書き両方を同時に
/// 拡張する。
///
/// 変更が 1 件以上あれば actor `Update` activity を 1 回だけフォロワーに配送
/// する (`local_api::profile::patch` / `actor lock`/`unlock` と共通の
/// [`build_update_activity`] + [`enqueue_to_followers`])。レスポンスは
/// `/api/i` と同じ `MeDetailed` ([`build_self_me_detailed`]) ── Aria の
/// `INotifier.setDescription` 等は応答を `MeDetailed.fromJson` でパースする
/// ため、最小 `{}` を返すと Dart の non-null field 検査に失敗して crash する
/// (= 本 endpoint 自体が無かった 404 と同じ症状に戻ってしまう)。
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<JsonValue>>,
) -> Response {
    let body = body.map_or(JsonValue::Null, |j| j.0);
    let token = body.get("i").and_then(JsonValue::as_str);
    let Some(_token_row) = auth::require_scope(&state, &headers, token, SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    let Some(local_actor) = resolve_self_actor(&state).await else {
        return internal_error("local actor not initialized; run `sakurasato init`");
    };

    let (patch, is_locked) = match parse_update_body(&state, &local_actor, &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let profile_changed = patch.has_changes();

    let mut actor = local_actor;
    if profile_changed {
        actor = match repo::actor::update_profile(state.pool(), actor.id, patch).await {
            Ok(row) => row,
            Err(err) => {
                tracing::error!(?err, "miauth i/update: update_profile failed");
                return internal_error("profile update failed");
            }
        };
    }

    let mut lock_changed = false;
    if let Some(want_locked) = is_locked
        && want_locked != actor.manually_approves_followers
    {
        actor =
            match repo::actor::set_manually_approves_followers(state.pool(), actor.id, want_locked)
                .await
            {
                Ok(row) => row,
                Err(err) => {
                    tracing::error!(
                        ?err,
                        "miauth i/update: set_manually_approves_followers failed"
                    );
                    return internal_error("profile update failed");
                }
            };
        lock_changed = true;
    }

    // プロフィール本体 + 鍵アカフラグ、どちらか一方でも変わったら actor
    // Update を 1 回だけ配送する (両方変わっても 2 回送らない ── フォロワーの
    // inbox を 2 倍叩く必要は無い)。
    if profile_changed || lock_changed {
        let activity = build_update_activity(&state, &actor).await;
        enqueue_to_followers(&state, &actor, &activity).await;
    }

    match build_self_me_detailed(&state).await {
        Ok(me) => Json(me).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /api/i/update` body から [`repo::actor::ProfilePatch`] と
/// `isLocked` を切り出す。[`update`] 本体からパース処理をまとめて分離した
/// だけの純粋なヘルパー (= clippy `too_many_lines` 対策)。
#[allow(
    clippy::result_large_err,
    reason = "text_field_change と同じ理由 ── Response は miauth 全体の共通 Err 型"
)]
async fn parse_update_body(
    state: &AppState,
    local_actor: &ActorRow,
    body: &JsonValue,
) -> Result<(repo::actor::ProfilePatch, Option<bool>), Response> {
    let patch = repo::actor::ProfilePatch {
        display_name: text_field_change(body, "name", DISPLAY_NAME_MAX)?,
        summary: text_field_change(body, "description", SUMMARY_MAX)?,
        icon_url: media_field_change(state, local_actor, body, "avatarId").await?,
        image_url: media_field_change(state, local_actor, body, "bannerId").await?,
        birthday: birthday_field_change(body)?,
        location: text_field_change(body, "location", LOCATION_MAX)?,
        lang: text_field_change(body, "lang", LANG_MAX)?,
        followed_message: text_field_change(body, "followedMessage", FOLLOWED_MESSAGE_MAX)?,
        fields: fields_change(body)?,
    };
    let is_locked = bool_field(body, "isLocked")?;
    Ok((patch, is_locked))
}

/// `body[key]` の三値状態を判定する (= フィールド省略 / 明示 `null` / 値)。
///
/// - キー不在 → `Ok(None)` (= 触らない)
/// - `null` → `Ok(Some(None))` (= クリア)
/// - 文字列 → trim して `max_chars` 以内なら `Ok(Some(Some(_)))`。trim 後
///   空文字は「クリア」と同義に倒す (= `local_api::profile::patch` と同じ
///   仕様。書き込み経路が違っても同じ列に対する意味を揃える)。
/// - それ以外の型 / 文字数超過 → `Err(400)`
#[allow(
    clippy::option_option,
    reason = "フィールド省略 / 明示 null / 値、の三値を表現する意図的な設計。\
              `local_api::profile::resolve_media_url` と同じ二重 Option パターン"
)]
#[allow(
    clippy::result_large_err,
    reason = "Response (axum) はこの miauth モジュール全体の共通 Err 型。\
              呼び出し側はすべて `Err(resp) => return resp` で即 return するだけなので\
              コピーコストは実害が無く、ここだけ Box で包むと非対称になる"
)]
fn text_field_change(
    body: &JsonValue,
    key: &str,
    max_chars: usize,
) -> Result<Option<Option<String>>, Response> {
    match body.get(key) {
        None => Ok(None),
        Some(JsonValue::Null) => Ok(Some(None)),
        Some(JsonValue::String(s)) => {
            if s.chars().count() > max_chars {
                return Err(bad_request(&format!(
                    "{key} exceeds the {max_chars}-character limit"
                )));
            }
            let trimmed = s.trim().to_string();
            Ok(Some((!trimmed.is_empty()).then_some(trimmed)))
        }
        Some(_) => Err(bad_request(&format!("{key} must be a string or null"))),
    }
}

/// `avatarId` / `bannerId` の三値状態を判定し、drive file id を実 URL に解決
/// する。
///
/// [`crate::local_api::profile::resolve_media_url`] (TUI 用 PATCH) と違って
/// `kind` (avatar/header/attachment) の一致は要求しない ──
/// [`crate::miauth::drive::create`] でアップロードされた drive file は常に
/// `kind = "attachment"` であり、Misskey 本家にも "kind" の区別自体が無い
/// (= 任意の drive file をそのまま avatarId/bannerId に指定できる仕様)。
/// 所有者ガードのみ課す。
async fn media_field_change(
    state: &AppState,
    local_actor: &ActorRow,
    body: &JsonValue,
    key: &str,
) -> Result<Option<Option<String>>, Response> {
    match body.get(key) {
        None => Ok(None),
        Some(JsonValue::Null) => Ok(Some(None)),
        Some(JsonValue::String(s)) => {
            let Ok(file_id) = s.parse::<i64>() else {
                return Err(error_resp(
                    StatusCode::NOT_FOUND,
                    "NO_SUCH_FILE",
                    "no such file",
                ));
            };
            match repo::media::get_by_id_for_owner(state.pool(), file_id, local_actor.id).await {
                Ok(Some(row)) => {
                    let host = &state.config().server.host;
                    Ok(Some(Some(build_media_url(host, &row.storage_key))))
                }
                Ok(None) => Err(error_resp(
                    StatusCode::NOT_FOUND,
                    "NO_SUCH_FILE",
                    "no such file",
                )),
                Err(err) => {
                    tracing::error!(?err, file_id, key, "miauth i/update: media lookup failed");
                    Err(internal_error("media lookup failed"))
                }
            }
        }
        Some(_) => Err(bad_request(&format!("{key} must be a string or null"))),
    }
}

/// `isLocked` 用の単純 bool フィールド判定。キー不在 → `None` (触らない)。
/// present なら bool 型必須 (Misskey wire は常に bool を送る)。
#[allow(
    clippy::result_large_err,
    reason = "text_field_change と同じ理由 ── Response は miauth 全体の共通 Err 型"
)]
fn bool_field(body: &JsonValue, key: &str) -> Result<Option<bool>, Response> {
    match body.get(key) {
        None => Ok(None),
        Some(JsonValue::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad_request(&format!("{key} must be a boolean"))),
    }
}

/// `birthday` の三値状態を判定する。Misskey wire は `"YYYY-MM-DD"` の ISO
/// 日付文字列 (時刻・タイムゾーンを持たない) ── `chrono::NaiveDate` として
/// 解釈できないものは 400 で弾く (= 壊れた日付文字列が DB に無検証で入るのを
/// 防ぐ)。有効な文字列はそのまま `actor.birthday` (TEXT) に保存する。
#[allow(
    clippy::option_option,
    reason = "text_field_change と同じ理由 ── フィールド省略 / 明示 null / 値、の三値を表現する"
)]
#[allow(
    clippy::result_large_err,
    reason = "text_field_change と同じ理由 ── Response は miauth 全体の共通 Err 型"
)]
fn birthday_field_change(body: &JsonValue) -> Result<Option<Option<String>>, Response> {
    match body.get("birthday") {
        None => Ok(None),
        Some(JsonValue::Null) => Ok(Some(None)),
        Some(JsonValue::String(s)) => {
            if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err() {
                return Err(bad_request("birthday must be an ISO date (YYYY-MM-DD)"));
            }
            Ok(Some(Some(s.clone())))
        }
        Some(_) => Err(bad_request("birthday must be a string or null")),
    }
}

/// `fields` の変更を判定する。Misskey wire は常に配列 (nullable ではない) な
/// ので、`text_field_change` 系と違い null は許容しない ── キー不在なら
/// `Ok(None)` (触らない)、配列なら要素を検証して丸ごと置換、それ以外は
/// 400。空配列 `[]` は「全項目クリア」として有効な値。
///
/// 各要素は `{ "name": string, "value": string }` (Misskey `UserField`)。
/// 件数・文字数の上限は Sakurasato 独自 (`FIELDS_MAX_COUNT` 等)。
#[allow(
    clippy::result_large_err,
    reason = "text_field_change と同じ理由 ── Response は miauth 全体の共通 Err 型"
)]
fn fields_change(body: &JsonValue) -> Result<Option<Vec<ActorField>>, Response> {
    let Some(value) = body.get("fields") else {
        return Ok(None);
    };
    let Some(arr) = value.as_array() else {
        return Err(bad_request("fields must be an array"));
    };
    if arr.len() > FIELDS_MAX_COUNT {
        return Err(bad_request(&format!(
            "fields must have at most {FIELDS_MAX_COUNT} entries"
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item.get("name").and_then(JsonValue::as_str);
        let value = item.get("value").and_then(JsonValue::as_str);
        let (Some(name), Some(value)) = (name, value) else {
            return Err(bad_request(
                "each fields entry must have string `name` and `value`",
            ));
        };
        if name.chars().count() > FIELD_NAME_MAX {
            return Err(bad_request(&format!(
                "fields.name exceeds the {FIELD_NAME_MAX}-character limit"
            )));
        }
        if value.chars().count() > FIELD_VALUE_MAX {
            return Err(bad_request(&format!(
                "fields.value exceeds the {FIELD_VALUE_MAX}-character limit"
            )));
        }
        out.push(ActorField {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    Ok(Some(out))
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn text_field_change_absent_key_is_none() {
        let body = serde_json::json!({});
        assert_eq!(text_field_change(&body, "description", 10).unwrap(), None);
    }

    #[test]
    fn text_field_change_null_clears() {
        let body = serde_json::json!({"description": null});
        assert_eq!(
            text_field_change(&body, "description", 10).unwrap(),
            Some(None)
        );
    }

    #[test]
    fn text_field_change_trims_and_sets() {
        let body = serde_json::json!({"description": "  hi  "});
        assert_eq!(
            text_field_change(&body, "description", 10).unwrap(),
            Some(Some("hi".to_string()))
        );
    }

    #[test]
    fn text_field_change_blank_after_trim_clears() {
        let body = serde_json::json!({"description": "   "});
        assert_eq!(
            text_field_change(&body, "description", 10).unwrap(),
            Some(None)
        );
    }

    #[test]
    fn text_field_change_rejects_too_long() {
        let body = serde_json::json!({"description": "x".repeat(11)});
        assert!(text_field_change(&body, "description", 10).is_err());
    }

    #[test]
    fn text_field_change_rejects_non_string() {
        let body = serde_json::json!({"description": 42});
        assert!(text_field_change(&body, "description", 10).is_err());
    }

    #[test]
    fn bool_field_absent_is_none() {
        let body = serde_json::json!({});
        assert_eq!(bool_field(&body, "isLocked").unwrap(), None);
    }

    #[test]
    fn bool_field_rejects_non_bool() {
        let body = serde_json::json!({"isLocked": "true"});
        assert!(bool_field(&body, "isLocked").is_err());
    }
}
