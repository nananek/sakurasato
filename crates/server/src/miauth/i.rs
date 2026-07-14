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

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::miauth::auth;
use crate::miauth::conv::from_actor_me_detailed;
use crate::miauth::meta::{build_policies, max_file_size_mb_from_bytes};
use crate::state::AppState;

/// `/api/i` の `read:account` scope (= Misskey 仕様の hardcoded constant)。
/// scope 文字列は `crate::miauth::auth::has_scope` で完全一致判定される。
const SCOPE_READ_ACCOUNT: &str = "read:account";

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

    let mut me = from_actor_me_detailed(&actor, followers, following, notes, policies);
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
