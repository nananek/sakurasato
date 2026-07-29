//! 受領 `Update` ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - `object.type` が Actor 系 (Person / Service / Application / Group /
//!   Organization) ─→ [`crate::remote_actor::fetch_and_upsert`] を再走させ、
//!   `actor` 行を最新値で書き直す (公開鍵ローテーションを含む)。
//! - `object.type` == `Note` ─→ 既知 `ap_id` を引いて content / summary /
//!   `edited_at` を上書き。既知 note の **作者本人** が編集している場合のみ
//!   受領 (F3 で body actor == signer 確認済み + ここで `note.actor_id` ==
//!   `signer.id` 検査)。
//! - それ以外の `object.type` は debug ログのみで no-op。
//!
//! ## 信頼境界
//!
//! - Actor 更新: `object.id` (= 更新対象 actor URI) が `signer.ap_id` と一致
//!   していなければ拒否 (= 他人の actor を勝手に refresh しようとする
//!   試みを防ぐ)。
//! - Note 編集: `note.actor_id` == `signer.id` 必須。Delete と同じ author check。

use anyhow::Context;
use chrono::Utc;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{debug, info, warn};

use super::DispatchError;
use crate::remote_actor;
use crate::state::AppState;

pub(crate) async fn handle_update(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    let obj = activity
        .get("object")
        .ok_or_else(|| DispatchError::Malformed("Update.object is required (inline)".into()))?;

    let JsonValue::Object(map) = obj else {
        return Err(DispatchError::Malformed(
            "Update.object must be an inline object (not a URI string)".into(),
        ));
    };

    let object_type = map
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DispatchError::Malformed("Update.object.type missing".into()))?;
    let object_id = map
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DispatchError::Malformed("Update.object.id missing".into()))?;

    if is_actor_type(object_type) {
        return update_actor(state, signer, object_id).await;
    }
    if object_type.eq_ignore_ascii_case("Note") {
        return update_note(state, signer, object_id, map).await;
    }

    debug!(
        object_type,
        signer = %signer.ap_id,
        "Update: unsupported object type; no-op",
    );
    Ok(())
}

fn is_actor_type(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "person" | "service" | "application" | "group" | "organization"
    )
}

async fn update_actor(
    state: &AppState,
    signer: &ActorRow,
    object_id: &str,
) -> Result<(), DispatchError> {
    // object.id が signer の ap_id と一致しないなら、別 actor の更新を
    // 押し付けてくる試行。F3 を通っていても危険なので拒否。
    if object_id != signer.ap_id {
        warn!(
            object_id,
            signer = %signer.ap_id,
            "Update Actor: object.id does not match signer; refusing",
        );
        return Err(DispatchError::Malformed(
            "Update Actor object.id does not match signer".into(),
        ));
    }

    // 既存 actor 行のフルセットを最新の actor JSON で書き直す。inline で
    // 来た object を信頼してパースする手もあるが、再 fetch のほうが
    // 鍵 owner / inbox URL 等の整合性検査を [`remote_actor`] と同じ規則で
    // やり直せて安全 (Mastodon は Update Actor の inline で `publicKey` を
    // 落とすことがある ── キーが消える書き換えを避けるため fetch を選ぶ)。
    let refreshed = remote_actor::fetch_and_upsert(state, &signer.ap_id)
        .await
        .with_context(|| format!("refetch actor {}", signer.ap_id))
        .map_err(DispatchError::Internal)?;

    info!(
        actor_id = refreshed.id,
        signer = %signer.ap_id,
        "actor profile refreshed from Update",
    );
    Ok(())
}

async fn update_note(
    state: &AppState,
    signer: &ActorRow,
    object_id: &str,
    obj: &serde_json::Map<String, JsonValue>,
) -> Result<(), DispatchError> {
    // F3 のネスト object 検査で attributedTo == signer は確認済み。
    // 念のため DB 上の note.actor_id と signer.id も照合する ── DB 行と
    // activity body が食い違ったら拒否 (本来あり得ないが防衛的に)。
    let Some(note) = repo::note::get_by_ap_id(state.pool(), object_id)
        .await
        .with_context(|| format!("lookup note {object_id}"))
        .map_err(DispatchError::Internal)?
    else {
        debug!(
            object_id,
            signer = %signer.ap_id,
            "Update Note: unknown note; no-op (we never had it)",
        );
        return Ok(());
    };

    if note.actor_id != signer.id {
        warn!(
            object_id,
            note_actor_id = note.actor_id,
            signer = %signer.ap_id,
            "Update Note: signer is not the note's author; refusing",
        );
        return Err(DispatchError::Malformed(
            "Update Note signer does not match note author".into(),
        ));
    }

    let new_content = obj
        .get("content")
        .and_then(JsonValue::as_str)
        .unwrap_or(&note.content)
        .to_string();
    // 空文字 / 空白のみ summary は CW なし (`None`) に正規化する ── Pleroma の
    // `summary: ""` が「空 CW あり」扱いにならないよう、Create 受信 (note.rs) と
    // 同じ [`super::note::normalize_summary`] を共有する。
    let new_summary =
        super::note::normalize_summary(obj.get("summary").and_then(JsonValue::as_str));

    // 長すぎる content / summary は弾く ── reaction と同じく DB 肥大対策。
    // local 投稿側の上限 (`local_api::notes`) と同じ 5000 / 200 char 上限を
    // 課す。
    if new_content.chars().count() > 5000 {
        return Err(DispatchError::Malformed(
            "Update Note content exceeds the 5000-character limit".into(),
        ));
    }
    if let Some(s) = new_summary.as_deref()
        && s.chars().count() > 200
    {
        return Err(DispatchError::Malformed(
            "Update Note summary exceeds the 200-character limit".into(),
        ));
    }

    // edited_at は object.updated を優先、無ければ受信時刻。
    let edited_at = obj
        .get("updated")
        .and_then(JsonValue::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc));

    let updated = repo::note::update_content(
        state.pool(),
        object_id,
        &new_content,
        new_summary.as_deref(),
        edited_at,
    )
    .await
    .with_context(|| format!("update note {object_id}"))
    .map_err(DispatchError::Internal)?;

    if let Some(row) = updated {
        // 編集後の本文中カスタム絵文字を学習する (note.rs::handle_create と
        // 同じ理由)。DB の `note.tags` 列自体は更新しない (本タスクのスコープ
        // 外、update_content の対象は content/summary/edited_at のみ) ので、
        // Update.object から都度読んで学習だけ行う。
        let emoji_summary = crate::emoji_learn::learn_note_emoji_tags(
            state,
            &signer.ap_id,
            obj.get("tag").unwrap_or(&JsonValue::Null),
        )
        .await;

        info!(
            note_id = row.id,
            signer = %signer.ap_id,
            edited_at = %edited_at,
            emoji_tags_seen = emoji_summary.emoji_tags_seen,
            emoji_learned = emoji_summary.emoji_learned,
            "remote note edited",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_type_matches_known_types() {
        assert!(is_actor_type("Person"));
        assert!(is_actor_type("person"));
        assert!(is_actor_type("Service"));
        assert!(is_actor_type("Application"));
        assert!(is_actor_type("Group"));
        assert!(is_actor_type("Organization"));
        assert!(!is_actor_type("Note"));
        assert!(!is_actor_type("Article"));
        assert!(!is_actor_type(""));
    }
}
