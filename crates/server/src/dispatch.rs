//! 受信した Activity の dispatch。
//!
//! `extract::SignedInboxBody` で署名検証を通過した body を JSON としてパースし、
//! `type` ごとに `handler::*` / `reaction::*` / `move_handler::*` に振る。
//! M3b-3 PR2 で Follow / Accept / Reject を、M8 PR2 で `Like` / `EmojiReact` /
//! `Undo` を、M9 で `Move` を実装した。`Create`/`Note` / `Announce` / `Update` /
//! `Delete` は後続 PR で順次対応する。
//!
//! ## F3: Activity body actor と署名者の一致 (PR #19 で挙がった必須項目)
//!
//! [`verify_body_actor`] が body の `actor` フィールドを `signed.actor.ap_id`
//! と完全一致させる。一致しない場合は 401 で拒否する。これを忘れると
//! `evil.example` の有効署名者が `good.example/users/...` を装った Activity を
//! 送り込め、F3 (= [[m3b-followup-plan]]) で明示された致命的 spoofing が成立する。
//!
//! `Create`/`Update`/`Delete` のネスト object (`Note.attributedTo` 等) も
//! 同じ規則で検証する ── [`verify_nested_object_actor`] が処理する。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use serde_json::Value as JsonValue;
use thiserror::Error;
use tracing::{info, warn};

use crate::state::AppState;

pub(crate) mod handler;
pub(crate) mod move_handler;
pub(crate) mod reaction;

/// dispatch エラー → HTTP レスポンス変換。署名検証 [`crate::sign::SigError`]
/// と同様に、検証失敗の詳細は body に書き戻さず汎用文言を返す。
#[derive(Debug, Error)]
pub(crate) enum DispatchError {
    /// body が JSON として解析できない。受信 inbox では構造的不備として 400。
    #[error("activity body is not valid JSON: {0}")]
    BadJson(#[from] serde_json::Error),

    /// `type` フィールドが無いか文字列でない。
    #[error("activity has no string `type` field")]
    MissingType,

    /// `actor` フィールドが無いか、文字列 URI または `{id: ...}` オブジェクト
    /// として解釈できない。
    #[error("activity has no usable `actor` field")]
    MissingActor,

    /// F3: 署名者の `ap_id` と body の actor が一致しない。
    /// **致命的なりすまし** なので 401 で拒否、ログには両方を残す。
    #[error("body actor {body_actor:?} does not match signing actor {signer:?}")]
    ActorMismatch { body_actor: String, signer: String },

    /// ネスト object (`Create.object.attributedTo` 等) の actor が署名者と
    /// 一致しない。
    #[error("nested object attributedTo {object_actor:?} does not match signing actor {signer:?}")]
    NestedObjectActorMismatch {
        object_actor: String,
        signer: String,
    },

    /// **Accept/Reject の signer が follow の followed actor と一致しない**:
    /// 例えば evil.example の actor が他人の follow に対する Accept を
    /// 送りつけてくるケース。F3 (body actor == signer) を通っていても
    /// handler 層で `followed_actor_id` 検査をすると `signer.id != followed_actor_id`
    /// で弾ける。401 で返す ── 503 にすると Mastodon が長めにリトライ保持
    /// するため、悪意ある actor からの偽 Accept 連投で帯域を奪われる
    /// (M3b-3 PR2 round-2 review F3)。
    #[error("Accept/Reject signer {signer:?} is not the followed actor of follow {follow_ap_id:?}")]
    UnrelatedAcceptor {
        signer: String,
        follow_ap_id: String,
    },

    /// 仕様外 / 受け入れ不能なフィールド構造。
    #[error("activity is malformed: {0}")]
    Malformed(String),

    /// DB アクセス・ネットワーク等の内部障害。503 で返す (Mastodon が長めに
    /// リトライ保持するため)。
    #[error("internal error during activity dispatch")]
    Internal(#[source] anyhow::Error),
}

impl DispatchError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadJson(_) | Self::MissingType | Self::MissingActor | Self::Malformed(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::ActorMismatch { .. }
            | Self::NestedObjectActorMismatch { .. }
            | Self::UnrelatedAcceptor { .. } => StatusCode::UNAUTHORIZED,
            Self::Internal(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for DispatchError {
    fn into_response(self) -> Response {
        // 401 / 400 の詳細はレスポンスに載せない。spoofing 攻撃者にどこで
        // 落ちたかの情報を返してしまうため。tracing には残す。
        match self.status() {
            StatusCode::SERVICE_UNAVAILABLE => {
                tracing::error!(error = %self, "inbox dispatch internal error");
                (StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response()
            }
            StatusCode::UNAUTHORIZED => {
                warn!(error = %self, "inbox dispatch rejected: actor mismatch");
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
            }
            _ => {
                warn!(error = %self, "inbox dispatch rejected: malformed activity");
                (StatusCode::BAD_REQUEST, "bad request").into_response()
            }
        }
    }
}

/// `actor` フィールドを文字列 URI で取り出す。
///
/// `ActivityPub` では `actor` は文字列 (URI) または `{"id": "...", "type": ...}`
/// オブジェクトのいずれか。両方に対応する。
fn extract_actor_uri(v: &JsonValue) -> Option<&str> {
    match v {
        JsonValue::String(s) => Some(s.as_str()),
        JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str),
        _ => None,
    }
}

/// F3: body の `actor` フィールドが署名者 `ap_id` と一致するか検証。
///
/// `signer` は extractor が DB から引いて返した actor の `ap_id`。比較は
/// 大文字小文字を区別する完全一致 (`ActivityPub` の id URI は case-sensitive)。
pub(crate) fn verify_body_actor(activity: &JsonValue, signer: &str) -> Result<(), DispatchError> {
    let actor_field = activity.get("actor").ok_or(DispatchError::MissingActor)?;
    let body_actor = extract_actor_uri(actor_field).ok_or(DispatchError::MissingActor)?;
    if body_actor != signer {
        return Err(DispatchError::ActorMismatch {
            body_actor: body_actor.to_string(),
            signer: signer.to_string(),
        });
    }
    Ok(())
}

/// `Create` / `Update` / `Delete` の `object` がネスト Activity / object の
/// `attributedTo` (または ネスト Activity 自身の `actor`) を持っていれば、
/// 署名者と一致することを確認する。
///
/// `object` が文字列 (URI 参照) の場合は検証スキップ。Mastodon の `Like`
/// などはこちら。
pub(crate) fn verify_nested_object_actor(
    activity: &JsonValue,
    signer: &str,
) -> Result<(), DispatchError> {
    let Some(obj) = activity.get("object") else {
        return Ok(());
    };
    let JsonValue::Object(map) = obj else {
        return Ok(());
    };
    // `attributedTo` (Note 等) と `actor` (ネストされた Activity) の両方を
    // 拾う。どちらか一方が一致しなければ拒否。
    for field in ["attributedTo", "actor"] {
        if let Some(v) = map.get(field)
            && let Some(other) = extract_actor_uri(v)
            && other != signer
        {
            return Err(DispatchError::NestedObjectActorMismatch {
                object_actor: other.to_string(),
                signer: signer.to_string(),
            });
        }
    }
    Ok(())
}

/// dispatch エントリポイント。
///
/// - body を JSON にパース
/// - F3 actor 一致を検証
/// - `type` ごとに handler に振る
/// - 未対応 type は 202 で受理 (連合相手の再送ループを起こさない)
pub(crate) async fn dispatch(
    state: &AppState,
    signer: &ActorRow,
    body: &[u8],
) -> Result<Response, DispatchError> {
    let activity: JsonValue = serde_json::from_slice(body)?;

    verify_body_actor(&activity, &signer.ap_id)?;
    verify_nested_object_actor(&activity, &signer.ap_id)?;

    let activity_type = activity
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or(DispatchError::MissingType)?
        .to_string();

    match activity_type.as_str() {
        "Follow" => {
            handler::handle_follow(state, signer, &activity)
                .await
                .map_err(DispatchError::Internal)?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "Accept" => {
            handler::handle_accept(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "Reject" => {
            handler::handle_reject(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "Like" => {
            reaction::handle_like(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "EmojiReact" => {
            reaction::handle_emoji_react(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "Undo" => {
            reaction::handle_undo(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        "Move" => {
            move_handler::handle_move(state, signer, &activity).await?;
            Ok((StatusCode::ACCEPTED, "accepted").into_response())
        }
        other => {
            info!(
                activity_type = other,
                signer = %signer.ap_id,
                "inbox dispatch: unsupported activity type accepted (no-op)",
            );
            Ok((StatusCode::ACCEPTED, "accepted; type not yet handled").into_response())
        }
    }
}

/// `ActivityPub` Activity body から `id` (= `ap_id`) を取り出す。
///
/// `Follow` / `Like` 等は `id` 必須 (`ActivityStreams` 仕様)。無い場合は
/// `Malformed` を返す。
pub(crate) fn extract_activity_id(activity: &JsonValue) -> Result<&str, DispatchError> {
    activity
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DispatchError::Malformed("activity has no string `id`".into()))
}

/// `object` フィールドから URI を取り出す (文字列 or `{id: ...}`)。
pub(crate) fn extract_object_uri(activity: &JsonValue) -> Result<&str, DispatchError> {
    let obj = activity
        .get("object")
        .ok_or_else(|| DispatchError::Malformed("activity has no `object`".into()))?;
    extract_actor_uri(obj)
        .ok_or_else(|| DispatchError::Malformed("activity `object` has no usable URI".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_actor_uri_handles_string_and_object() {
        let s = json!("https://x.test/users/a");
        assert_eq!(extract_actor_uri(&s), Some("https://x.test/users/a"));

        let o = json!({"id": "https://x.test/users/b", "type": "Person"});
        assert_eq!(extract_actor_uri(&o), Some("https://x.test/users/b"));

        // null / number / array は無効。
        assert_eq!(extract_actor_uri(&JsonValue::Null), None);
        assert_eq!(extract_actor_uri(&json!(42)), None);
        assert_eq!(extract_actor_uri(&json!([])), None);

        // object に id が無いと無効。
        assert_eq!(extract_actor_uri(&json!({"type": "Person"})), None);
    }

    #[test]
    fn verify_body_actor_accepts_matching_string() {
        let a = json!({
            "type": "Follow",
            "actor": "https://evil.example/users/x",
            "object": "https://x.test/users/me",
        });
        verify_body_actor(&a, "https://evil.example/users/x").unwrap();
    }

    #[test]
    fn verify_body_actor_accepts_matching_object_form() {
        let a = json!({
            "type": "Follow",
            "actor": {"id": "https://evil.example/users/x", "type": "Person"},
            "object": "https://x.test/users/me",
        });
        verify_body_actor(&a, "https://evil.example/users/x").unwrap();
    }

    #[test]
    fn verify_body_actor_rejects_spoofed_actor() {
        // F3: 署名者 `evil.example/users/x` が body の actor だけ
        // `good.example/users/...` に差し替えてもここで止まる。
        let a = json!({
            "type": "Follow",
            "actor": "https://good.example/users/victim",
            "object": "https://x.test/users/me",
        });
        let err = verify_body_actor(&a, "https://evil.example/users/x").unwrap_err();
        assert!(matches!(err, DispatchError::ActorMismatch { .. }));
    }

    #[test]
    fn verify_body_actor_rejects_missing_actor() {
        let a = json!({"type": "Follow"});
        assert!(matches!(
            verify_body_actor(&a, "https://x.test/u/a").unwrap_err(),
            DispatchError::MissingActor
        ));
    }

    #[test]
    fn verify_nested_object_actor_checks_attributed_to() {
        // Create {Note { attributedTo: ... }} の検証。
        let a = json!({
            "type": "Create",
            "actor": "https://x.test/users/alice",
            "object": {
                "type": "Note",
                "attributedTo": "https://x.test/users/alice",
                "content": "hi",
            },
        });
        verify_nested_object_actor(&a, "https://x.test/users/alice").unwrap();

        // attributedTo が別人になっているケース。
        let bad = json!({
            "type": "Create",
            "actor": "https://x.test/users/alice",
            "object": {
                "type": "Note",
                "attributedTo": "https://other.test/users/bob",
                "content": "hi",
            },
        });
        let err = verify_nested_object_actor(&bad, "https://x.test/users/alice").unwrap_err();
        assert!(matches!(
            err,
            DispatchError::NestedObjectActorMismatch { .. }
        ));
    }

    #[test]
    fn verify_nested_object_actor_checks_nested_activity_actor() {
        // Undo {Follow { actor: ... }} 形式の検証。
        let a = json!({
            "type": "Undo",
            "actor": "https://x.test/users/alice",
            "object": {
                "type": "Follow",
                "actor": "https://x.test/users/alice",
                "object": "https://other.test/users/bob",
            },
        });
        verify_nested_object_actor(&a, "https://x.test/users/alice").unwrap();
    }

    #[test]
    fn verify_nested_object_actor_allows_string_object_reference() {
        // Like {object: "<URI>"} のように object が文字列参照ならスキップ。
        let a = json!({
            "type": "Like",
            "actor": "https://x.test/users/alice",
            "object": "https://other.test/notes/1",
        });
        verify_nested_object_actor(&a, "https://x.test/users/alice").unwrap();
    }

    #[test]
    fn verify_nested_object_actor_allows_missing_object() {
        let a = json!({
            "type": "Delete",
            "actor": "https://x.test/users/alice",
        });
        verify_nested_object_actor(&a, "https://x.test/users/alice").unwrap();
    }
}
