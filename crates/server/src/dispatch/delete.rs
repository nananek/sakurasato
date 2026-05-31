//! 受領 `Delete` ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - 対象: 既知の **リモート Note** の削除のみ。
//! - 未知 `ap_id` は no-op (= 持っていない note を消せと言われても落ち
//!   ないように)。
//! - `Tombstone` ラッパ・inline `Note` オブジェクト・URI 文字列のいずれの
//!   `object` 形態にも対応。
//!
//! ## 信頼境界
//!
//! - F3 (body actor == signer) は [`super::dispatch`] が確認済み。
//! - **追加検証**: `note.actor_id == signer.id` ── 第三者削除を許さない。
//!   Mastodon / Misskey も「他人の Note を別 actor 名義で消せる」は致命的
//!   なので、必ずチェック。
//! - 削除権限が無い場合は `DispatchError::Malformed` で 400 を返す
//!   (= retry に乗らない)。なりすまし試行扱い。

use anyhow::Context;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{info, warn};

use super::DispatchError;
use crate::state::AppState;

pub(crate) async fn handle_delete(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    let target_ap_id = extract_delete_target(activity)?;

    let Some(note) = repo::note::get_by_ap_id(state.pool(), &target_ap_id)
        .await
        .with_context(|| format!("lookup note {target_ap_id}"))
        .map_err(DispatchError::Internal)?
    else {
        // 未知 Note の Delete = 我々が取り込まなかった投稿の削除通知。
        // Mastodon の lemmy 連合などで forwarding により頻繁に流れてくるので、
        // info ではなく debug で抑える。
        tracing::debug!(
            target = %target_ap_id,
            signer = %signer.ap_id,
            "Delete: unknown note; no-op"
        );
        return Ok(());
    };

    if note.actor_id != signer.id {
        warn!(
            target = %target_ap_id,
            note_id = note.id,
            note_actor_id = note.actor_id,
            signer = %signer.ap_id,
            "Delete: signer is not the note's author; refusing",
        );
        return Err(DispatchError::Malformed(
            "Delete signer does not match note author".into(),
        ));
    }

    let deleted = repo::note::delete_by_ap_id(state.pool(), &target_ap_id)
        .await
        .with_context(|| format!("delete note {target_ap_id}"))
        .map_err(DispatchError::Internal)?;
    info!(
        target = %target_ap_id,
        note_id = note.id,
        signer = %signer.ap_id,
        deleted,
        "note deleted by author",
    );
    Ok(())
}

/// `Delete.object` から target AP id を取り出す。
///
/// 受け付ける形態:
/// - 文字列 (= URI そのもの): `{object: "https://x/notes/1"}`
/// - inline オブジェクト + `id`: `{object: {type: "Note", id: "..."}}` /
///   `{object: {type: "Tombstone", id: "..."}}`
///
/// AP 仕様上 `Tombstone` は削除済みオブジェクトの placeholder type で、
/// Mastodon は削除通知に Tombstone を入れる。Misskey は文字列 URI を投げる
/// ことが多い。両方サポートする。
fn extract_delete_target(activity: &JsonValue) -> Result<String, DispatchError> {
    let obj = activity
        .get("object")
        .ok_or_else(|| DispatchError::Malformed("Delete has no `object`".into()))?;
    match obj {
        JsonValue::String(s) => Ok(s.clone()),
        JsonValue::Object(map) => map
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| DispatchError::Malformed("Delete.object has no string `id`".into())),
        _ => Err(DispatchError::Malformed(
            "Delete.object is neither a URI string nor an object".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_target_from_uri_string() {
        let a = json!({"type": "Delete", "object": "https://x.test/notes/1"});
        assert_eq!(extract_delete_target(&a).unwrap(), "https://x.test/notes/1");
    }

    #[test]
    fn extract_target_from_tombstone() {
        let a = json!({
            "type": "Delete",
            "object": {"type": "Tombstone", "id": "https://x.test/notes/2"},
        });
        assert_eq!(extract_delete_target(&a).unwrap(), "https://x.test/notes/2");
    }

    #[test]
    fn extract_target_from_inline_note() {
        let a = json!({
            "type": "Delete",
            "object": {"type": "Note", "id": "https://x.test/notes/3"},
        });
        assert_eq!(extract_delete_target(&a).unwrap(), "https://x.test/notes/3");
    }

    #[test]
    fn extract_target_rejects_missing_object() {
        let a = json!({"type": "Delete"});
        assert!(matches!(
            extract_delete_target(&a).unwrap_err(),
            DispatchError::Malformed(_)
        ));
    }

    #[test]
    fn extract_target_rejects_object_without_id() {
        let a = json!({"type": "Delete", "object": {"type": "Note"}});
        assert!(matches!(
            extract_delete_target(&a).unwrap_err(),
            DispatchError::Malformed(_)
        ));
    }
}
