//! `MiAuth` 用 認証 + permission scope 検証ヘルパ (M14 #157)。
//!
//! Misskey 互換クライアントの wire 形式に合わせて **2 経路** をサポートする
//! 設計だが、#157 (foundation) では handler 経路の helper まで提供し、
//! 実 endpoint からの呼び出しは #158 以降で接続される。
//!
//! ## 認証経路の比較
//!
//! | source                       | endpoint 例                     | 取り出し方         |
//! |------------------------------|---------------------------------|--------------------|
//! | body `i` フィールド (主流)   | `POST /api/i { i: <token> }`    | handler 側で parse |
//! | `Authorization: Bearer ...`  | (互換のため受理する)            | header から parse  |
//!
//! Misskey 公式の wire 形式は **body `i`** で、Misskey クライアント (Milktea /
//! `MissRirica` / `MiPA`) は全部これを使う。`Authorization: Bearer` は `MiAuth` では
//! 公式仕様に含まれていないが、多くの fork (Iceshrimp / Sharkey) が同等の互換
//! 受け口を持つので、Sakurasato も両受けにする (= 既存 `local_api` の `Bearer`
//! 利用者からの cross-test も容易になる)。
//!
//! ## #157 で提供するもの
//!
//! - [`validate_token_raw`] — 生トークン文字列を hash 化して DB lookup する
//!   コアロジック (失敗時は best-effort logging)。
//! - [`mark_used_async`] — `last_used_at` を best-effort 更新する detached task。
//! - [`has_scope`] — Token row が指定 scope を持つか判定。
//! - [`require_scope`] / [`validate_token_for_scope`] — token 解決 + scope 検査を
//!   1 関数で行う共通ヘルパ。`ignore_scope` (アナーキー) ON なら scope 検査を
//!   スキップし、OFF なら token 有効 = `Ok` / scope 不足 = [`MiAuthScopeError`]
//!   (401 vs 403 を区別)。
//! - [`unauthorized`] / [`forbidden`] — error response の組立 helper (Misskey
//!   wire の `id` / `kind` 込み)。
//! - [`parse_bearer_header`] — `Authorization: Bearer ...` の薄いラッパ
//!   (= 既存 [`crate::token::parse_bearer`] を再 export している実装)。
//!
//! アナーキーフラグ (`config.miauth.ignore_scope`, default ON) の詳細は
//! [`require_scope`] のドキュメントを参照。

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::MiAuthTokenRow;
use sakurasato_core::repo;
use serde_json::json;
use uuid::Uuid;

use crate::miauth::error::error_kind;
use crate::state::AppState;

/// 生トークン文字列 → DB lookup。成功なら [`MiAuthTokenRow`] を返す。
///
/// 失敗時 (= 該当 token が無い、or DB エラー):
/// - **None** (= token unknown) は呼び出し側で 401 に倒す
/// - DB エラーは `tracing::error!` で記録した上で None を返す (= 401 と同じ
///   レスポンスにすることで Misskey クライアントに「token は間違いだろう」
///   と再認可フローを案内する。500 を返すと client がリトライループに陥り
///   やすい)
pub async fn validate_token_raw(state: &AppState, raw: &str) -> Option<MiAuthTokenRow> {
    let hash = crate::token::hash(raw);
    match repo::miauth::find_token_by_hash(state.pool(), &hash).await {
        Ok(Some(row)) => Some(row),
        Ok(None) => None,
        Err(err) => {
            tracing::error!(?err, "miauth_token lookup failed");
            None
        }
    }
}

/// `last_used_at` の best-effort 更新を別タスクに投げる。
/// 呼び出し側はブロックせずすぐ return できる ── 失敗しても warn ログだけで
/// リクエスト本体には影響しない (= 既存 `api_token` の middleware と同じ方針)。
pub fn mark_used_async(state: &AppState, token_id: i64) {
    let task_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = repo::miauth::mark_token_used(task_state.pool(), token_id).await {
            tracing::warn!(?err, token_id, "miauth_token mark_used failed");
        }
    });
}

/// `token` が指定 scope を持つか判定する。完全一致 ── prefix 拡張 (= `write:*`
/// で `write:notes` も含む等) は **行わない**。Misskey の scope は階層を持た
/// ない flat な string set として扱われており、prefix matching を入れると
/// 意図しない権限拡張が発生しうるため。
///
/// 呼び出し側は要求 scope を hardcoded constant として渡す:
/// ```ignore
/// const SCOPE_READ_ACCOUNT: &str = "read:account";
/// if !has_scope(&row, SCOPE_READ_ACCOUNT) {
///     return forbidden("missing scope: read:account");
/// }
/// ```
pub fn has_scope(token: &MiAuthTokenRow, required: &str) -> bool {
    token.permissions.0.iter().any(|s| s == required)
}

/// 401 レスポンス (= 認証情報が無い / 不正)。`WWW-Authenticate` ヘッダで
/// `MiAuth` scheme を案内する (= Misskey クライアントは Bearer と body `i` の
/// 両方を試すので、scheme 名は将来の拡張用 informational に近い)。
///
/// body は Misskey wire の **nested** 形 `{"error":{"code","message","id","kind"}}`
/// で返す (#197 / fix/miauth-follow-failure) ── `forbidden` (`PERMISSION_DENIED`)
/// や各 endpoint の 404/500 と shape を揃え、クライアントが 401 から
/// `code`/`message` を取り出せるようにする。`id` は misskey-dart の
/// `MisskeyException.fromJson` が required で読む UUID (= 欠落すると client が
/// raw `DioException` に倒れてエラー表示が壊れる)。code は
/// `AUTHENTICATION_FAILED` (= Misskey が無効トークンに返す wire code)。
pub fn unauthorized(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Bearer realm="sakurasato""#)],
        Json(json!({
            "error": {
                "code": "AUTHENTICATION_FAILED",
                "message": reason,
                "id": Uuid::new_v4().to_string(),
                "kind": error_kind(StatusCode::UNAUTHORIZED, "AUTHENTICATION_FAILED"),
            }
        })),
    )
        .into_response()
}

/// 403 レスポンス (= 認証は通ったが scope が足りない)。Misskey 系の慣習で
/// `error.code = "PERMISSION_DENIED"` 相当を返すと client 側でわかりやすい
/// (= scope 不足の hint UI が出る) ので body にも入れておく。`kind = "permission"`
/// は fork の `MisskeyExceptionKind` enum に一致させる。
pub fn forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": {
                "code": "PERMISSION_DENIED",
                "message": reason,
                "id": Uuid::new_v4().to_string(),
                "kind": error_kind(StatusCode::FORBIDDEN, "PERMISSION_DENIED"),
            }
        })),
    )
        .into_response()
}

/// `Authorization: Bearer <token>` ヘッダ値から token 部分を取り出す
/// (= 既存 [`crate::token::parse_bearer`] への薄い再 export)。
/// `MiAuth` が Bearer も受けるための入り口。
pub fn parse_bearer_header(header_value: &str) -> Option<&str> {
    crate::token::parse_bearer(header_value)
}

/// `body.i` → `Authorization: Bearer` の順で raw token を取り出し、DB lookup +
/// scope 検査までを 1 関数で済ませる共通ヘルパ (= PR #165 round-2 review #1
/// の duplication 解消)。
///
/// 成功時 (= token 解決 + scope OK) は `Ok(MiAuthTokenRow)` を返し、
/// 同時に `last_used_at` の best-effort 更新タスクを spawn する。
///
/// 失敗時は [`MiAuthScopeError`] で区別する:
///
/// - [`MiAuthScopeError::Unauthorized`]: token が無い / 不正 / revoke 済み
///   (= 401 `AUTHENTICATION_FAILED` に倒す)。
/// - [`MiAuthScopeError::Forbidden`]: token は有効だが要求 scope が無い
///   (= 403 `PERMISSION_DENIED` に倒す。**`ignore_scope` (= アナーキー) OFF
///   のときのみ発生**。ON なら scope 検査自体をスキップする)。
///
/// ## アナーキーフラグ (`ignore_scope`)
///
/// `config.miauth.ignore_scope` (default ON) が `true` のとき、scope 検査を
/// **丸ごとスキップ**する ── token が有効 (= hash が DB に存在) なら
/// `permissions` 列を参照せず全 read/write を許可する。お一人様サーバの実態に
/// 合わせ、Aria 等の follow が `write:following` scope の grant 忘れで 401 に
/// なる問題 (= 本 task) を構造的に解消する。**token 有効性の検査は常に残す**
/// (= 無効 / revoke 済み token の 401 は維持)。
pub async fn require_scope(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    body_i: Option<&str>,
    required_scope: &str,
) -> Result<MiAuthTokenRow, MiAuthScopeError> {
    let raw = match body_i.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_bearer_header)
            .map(str::to_string)
            .ok_or(MiAuthScopeError::Unauthorized)?,
    };
    validate_token_for_scope(state, &raw, required_scope).await
}

/// `require_scope` のうち「raw token 確定後」の部分。
///
/// token 有効性 (= hash lookup) は常に検査し、`ignore_scope` (アナーキー) ON
/// なら scope 検査をスキップする。i.rs の `handle` / streaming.rs のように
/// body `i` / query `i` の取り出し方が `require_scope` と異なる経路からも
/// 同じ検証ロジックを共有できるよう切り出した。
pub async fn validate_token_for_scope(
    state: &AppState,
    raw: &str,
    required_scope: &str,
) -> Result<MiAuthTokenRow, MiAuthScopeError> {
    // token 有効性はアナーキーでも常に検査 (= 無効 / revoke 済みは 401)。
    let Some(token_row) = validate_token_raw(state, raw).await else {
        return Err(MiAuthScopeError::Unauthorized);
    };
    if state
        .config()
        .miauth
        .as_ref()
        .is_some_and(|m| m.ignore_scope)
    {
        mark_used_async(state, token_row.id);
        return Ok(token_row);
    }
    if !has_scope(&token_row, required_scope) {
        return Err(MiAuthScopeError::Forbidden);
    }
    mark_used_async(state, token_row.id);
    Ok(token_row)
}

/// [`require_scope`] / [`validate_token_for_scope`] の失敗種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiAuthScopeError {
    /// token が無い / 不正 / revoke 済み → 401 `AUTHENTICATION_FAILED`。
    Unauthorized,
    /// token は有効だが要求 scope が無い → 403 `PERMISSION_DENIED`
    /// (= `ignore_scope` OFF / 厳密モード時のみ到達)。
    Forbidden,
}

impl MiAuthScopeError {
    /// Misskey wire のエラーレスポンスへ変換する。
    pub fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => unauthorized("invalid or revoked token"),
            Self::Forbidden => forbidden("missing required scope"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sakurasato_core::model::MiAuthTokenRow;
    use sqlx::types::Json as SqlxJson;

    fn fake_token_row(perms: Vec<&str>) -> MiAuthTokenRow {
        MiAuthTokenRow {
            id: 1,
            name: "test".into(),
            token_hash: "0".into(),
            permissions: SqlxJson(perms.into_iter().map(String::from).collect()),
            last_used_at: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn has_scope_exact_match() {
        let row = fake_token_row(vec!["read:account", "write:reactions"]);
        assert!(has_scope(&row, "read:account"));
        assert!(has_scope(&row, "write:reactions"));
        assert!(!has_scope(&row, "write:notes"));
    }

    /// `write:notes` を持つ token で `write:*` のような prefix 一致は許可
    /// **しない** (= 意図しない権限拡張を防ぐ)。Misskey の scope は flat。
    #[test]
    fn has_scope_no_prefix_matching() {
        let row = fake_token_row(vec!["write:notes"]);
        assert!(!has_scope(&row, "write:reactions"));
        assert!(!has_scope(&row, "write"));
        assert!(!has_scope(&row, "write:notes:extra"));
    }

    /// 空の permissions では何の scope も持たない。
    #[test]
    fn has_scope_empty_grants_nothing() {
        let row = fake_token_row(vec![]);
        assert!(!has_scope(&row, "read:account"));
    }

    #[test]
    fn parse_bearer_header_delegates() {
        assert_eq!(parse_bearer_header("Bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer_header("bearer  xyz"), Some("xyz"));
        assert_eq!(parse_bearer_header("xyz"), None);
        assert_eq!(parse_bearer_header(""), None);
    }
}
