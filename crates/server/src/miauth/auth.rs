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
//! - [`unauthorized`] / [`forbidden`] — error response の組立 helper。
//! - [`parse_bearer_header`] — `Authorization: Bearer ...` の薄いラッパ
//!   (= 既存 [`crate::token::parse_bearer`] を再 export している実装)。
//!
//! #158 では `POST /api/i` 等で本ヘルパを使い、handler 内で:
//! ```ignore
//! let raw = body.i.or_else(|| parse_bearer_header(&headers));
//! let row = validate_token_raw(&state, &raw).await?;
//! if !has_scope(&row, "read:account") { return forbidden("missing scope"); }
//! mark_used_async(&state, row.id);
//! // ... handler 本体
//! ```
//! のような flow に組む。

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::MiAuthTokenRow;
use sakurasato_core::repo;
use serde_json::json;

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
pub fn unauthorized(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Bearer realm="sakurasato""#)],
        Json(json!({"error": reason})),
    )
        .into_response()
}

/// 403 レスポンス (= 認証は通ったが scope が足りない)。Misskey 系の慣習で
/// `error.code = "PERMISSION_DENIED"` 相当を返すと client 側でわかりやすい
/// (= scope 不足の hint UI が出る) ので body にも入れておく。
pub fn forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": {
                "code": "PERMISSION_DENIED",
                "message": reason,
            },
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
/// 成功時 (= token 解決 + scope OK) は `Some(MiAuthTokenRow)` を返し、
/// 同時に `last_used_at` の best-effort 更新タスクを spawn する。
///
/// 失敗時は `None` を返す。呼び出し側は `None` を見たら
/// [`unauthorized`] を返す ── token 未提示 / 不正 / scope 不足 のいずれも
/// 「401 unauthorized」に倒す。Misskey wire 仕様は 401/403 を厳密には区別
/// しない (= client は再認可フローに倒すだけ) ので、外側からは 1 種類で扱う。
///
/// **scope 不足** を明示したいときは戻り値の `MiAuthTokenRow.permissions` を
/// 別途引いて [`forbidden`] を返す経路が必要だが、本ヘルパは「scope OK」を
/// boolean に潰す。
pub async fn require_scope(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    body_i: Option<&str>,
    required_scope: &str,
) -> Option<MiAuthTokenRow> {
    let raw = match body_i.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_bearer_header)
            .map(str::to_string)?,
    };
    let token_row = validate_token_raw(state, &raw).await?;
    if !has_scope(&token_row, required_scope) {
        return None;
    }
    mark_used_async(state, token_row.id);
    Some(token_row)
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
