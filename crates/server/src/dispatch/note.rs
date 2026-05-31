//! 受領 `Create` ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - 対象: inline `Note` を持つ Create のみ (`Article` 等は no-op)。
//! - 受け入れ条件:
//!   - **signer が我々の followee** (= local actor が signer を `accepted`
//!     で follow している) のとき。タイムラインに流す前提。
//!   - **または** activity / object の `to` / `cc` に我々の local actor が
//!     明示されている (= mention / reply / DM)。
//! - 上記いずれも満たさない post は debug ログのみで no-op ── public TL に
//!   流れる無関係 post を引き込むと DB が肥大するため (CLAUDE.md §5.1 / #55)。
//! - 重複 `ap_id` (= retry / duplicate delivery) は info で no-op。
//!
//! ## 信頼境界
//!
//! - F3 (body actor == signer) と nested object `attributedTo` 検査は
//!   [`super::dispatch`] で通過済み。ここでは signer が note の作者本人と
//!   みなしてよい。
//! - `Note.id` のホストが signer のホストと一致することは追加で検証する
//!   (= ホスト混入 spoofing 防御)。

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use sakurasato_core::model::{ActorRow, Visibility};
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{debug, info};
use url::Url;

use super::DispatchError;
use crate::state::AppState;

/// AS2 の "public" 配送先 magic URI。
const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";

/// Note の content / summary の保護的上限 (DB 肥大対策)。local 投稿側の
/// 上限 (`local_api::notes`) と同じ値。リモートが滅多に超えないが、
/// `aaaa...` を流し込まれて DB を膨らませない保険。
const CONTENT_MAX: usize = 5000;
const SUMMARY_MAX: usize = 200;

pub(crate) async fn handle_create(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    let Some(JsonValue::Object(obj)) = activity.get("object") else {
        return Err(DispatchError::Malformed(
            "Create.object must be an inline object".into(),
        ));
    };

    let object_type = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if !object_type.eq_ignore_ascii_case("Note") {
        debug!(
            object_type,
            signer = %signer.ap_id,
            "Create: object is not a Note; ignoring",
        );
        return Ok(());
    }

    let note_ap_id = obj
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DispatchError::Malformed("Create.object.id missing".into()))?
        .to_string();

    same_host(&note_ap_id, &signer.ap_id, "Note id")
        .map_err(|e| DispatchError::Malformed(format!("{e:#}")))?;

    // 重複 (= retry / forwarding) は何も触らずに 202。
    if repo::note::get_by_ap_id(state.pool(), &note_ap_id)
        .await
        .with_context(|| format!("lookup note {note_ap_id}"))
        .map_err(DispatchError::Internal)?
        .is_some()
    {
        debug!(
            note_ap_id = %note_ap_id,
            signer = %signer.ap_id,
            "Create: note already stored; ignoring duplicate",
        );
        return Ok(());
    }

    let recipients = collect_recipients(obj, activity);
    let local_ap_id = format!(
        "https://{host}/users/{user}",
        host = state.config().server.host,
        user = state.config().server.user,
    );
    let addresses_us = recipients.iter_all().any(|r| r == &local_ap_id);
    let followed = !repo::follow::list_local_following(state.pool(), signer.id)
        .await
        .context("list_local_following for Create filter")
        .map_err(DispatchError::Internal)?
        .is_empty();

    if !addresses_us && !followed {
        debug!(
            note_ap_id = %note_ap_id,
            signer = %signer.ap_id,
            "Create: not from a followee and we are not addressed; ignoring",
        );
        return Ok(());
    }

    let new = build_remote_note(state, signer, &note_ap_id, obj, &recipients).await?;
    let inserted = repo::note::insert(state.pool(), new)
        .await
        .with_context(|| format!("insert remote note {note_ap_id}"))
        .map_err(DispatchError::Internal)?;

    info!(
        note_id = inserted.id,
        note_ap_id = %note_ap_id,
        signer = %signer.ap_id,
        visibility = %inserted.visibility,
        addresses_us,
        followed,
        "remote note stored",
    );
    Ok(())
}

/// `to` / `cc` の集合。activity 階層と object 階層の両方を保持して、
/// visibility 推定にも mention 判定にも使い回す。
struct Recipients {
    object_to: Vec<String>,
    object_cc: Vec<String>,
    activity_to: Vec<String>,
    activity_cc: Vec<String>,
}

impl Recipients {
    fn iter_all(&self) -> impl Iterator<Item = &String> {
        self.object_to
            .iter()
            .chain(self.object_cc.iter())
            .chain(self.activity_to.iter())
            .chain(self.activity_cc.iter())
    }
}

fn collect_recipients(
    obj: &serde_json::Map<String, JsonValue>,
    activity: &JsonValue,
) -> Recipients {
    Recipients {
        object_to: extract_string_array(obj.get("to")),
        object_cc: extract_string_array(obj.get("cc")),
        activity_to: extract_string_array(activity.get("to")),
        activity_cc: extract_string_array(activity.get("cc")),
    }
}

/// `Create.object` (= inline Note) を [`repo::note::NewNote`] に詰める。
/// 上限 / `inReplyTo` 解決 / visibility 推定をここでまとめて行う。
async fn build_remote_note(
    state: &AppState,
    signer: &ActorRow,
    note_ap_id: &str,
    obj: &serde_json::Map<String, JsonValue>,
    recipients: &Recipients,
) -> Result<repo::note::NewNote, DispatchError> {
    let content = obj
        .get("content")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    if content.chars().count() > CONTENT_MAX {
        return Err(DispatchError::Malformed(
            "Create.Note content exceeds the 5000-character limit".into(),
        ));
    }

    let summary = obj
        .get("summary")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    if let Some(s) = summary.as_deref()
        && s.chars().count() > SUMMARY_MAX
    {
        return Err(DispatchError::Malformed(
            "Create.Note summary exceeds the 200-character limit".into(),
        ));
    }

    let language = obj
        .get("language")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let in_reply_to_ap_id = obj
        .get("inReplyTo")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let in_reply_to_note_id = if let Some(uri) = in_reply_to_ap_id.as_deref() {
        // DB 接続障害 (= Internal) と「親 note が未知」(= Ok(None)) を区別する。
        // `.ok()` で平滑化すると接続障害時に reply 無し扱いで insert が成立し、
        // インフラ問題が静かにすり抜ける ([[m11-pr-review]] minor 1)。
        repo::note::get_by_ap_id(state.pool(), uri)
            .await
            .with_context(|| format!("lookup reply parent {uri}"))
            .map_err(DispatchError::Internal)?
            .map(|n| n.id)
    } else {
        None
    };
    let visibility = derive_visibility(
        &recipients.object_to,
        &recipients.object_cc,
        &recipients.activity_to,
        &recipients.activity_cc,
    );
    let sensitive = obj
        .get("sensitive")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let url = obj.get("url").and_then(extract_url_string);
    let published_at = obj
        .get("published")
        .and_then(JsonValue::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc));
    let attachments = obj
        .get("attachment")
        .cloned()
        .unwrap_or_else(|| JsonValue::Array(vec![]));
    let tags = obj
        .get("tag")
        .cloned()
        .unwrap_or_else(|| JsonValue::Array(vec![]));

    Ok(repo::note::NewNote {
        ap_id: note_ap_id.to_string(),
        actor_id: signer.id,
        content,
        language,
        in_reply_to_ap_id,
        in_reply_to_note_id,
        summary,
        visibility,
        sensitive,
        to_recipients: recipients.object_to.clone(),
        cc_recipients: recipients.object_cc.clone(),
        attachments,
        tags,
        is_local: false,
        url,
        published_at,
    })
}

/// `to` / `cc` などの string array 抽出。文字列以外の要素は無視。
fn extract_string_array(v: Option<&JsonValue>) -> Vec<String> {
    let Some(JsonValue::Array(arr)) = v else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(JsonValue::as_str)
        .map(str::to_string)
        .collect()
}

/// `url` フィールドは文字列単体、または `{type: "Link", href: "..."}` 形式、
/// または `[{...}, "string"]` の配列がある。最初に取れた URI を返す。
fn extract_url_string(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Object(map) => map
            .get("href")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        JsonValue::Array(arr) => arr.iter().find_map(extract_url_string),
        _ => None,
    }
}

/// AP の `to`/`cc` 配列から DB の `visibility` 列の値を推測する。
///
/// CLAUDE.md M4 PR2 仕様の逆方向:
/// - to に Public      → public
/// - cc に Public      → unlisted
/// - そのどちらでもないが to/cc に followers 系 URL がある → followers
/// - それ以外          → direct (= 我々宛のみの DM 等)
///
/// 厳密判定 (= 何が followers URL か signer 側で確定) は handler 段では
/// 難しいので、`public` / `unlisted` / `followers` / `direct` への ざっくり
/// 分類で十分。後段の表示で困らない粒度。
fn derive_visibility(
    object_to: &[String],
    object_cc: &[String],
    activity_to: &[String],
    activity_cc: &[String],
) -> Visibility {
    let any_to = object_to.iter().chain(activity_to.iter());
    let any_cc = object_cc.iter().chain(activity_cc.iter());

    if any_to.clone().any(|r| r == PUBLIC_URI) {
        Visibility::Public
    } else if any_cc.clone().any(|r| r == PUBLIC_URI) {
        Visibility::Unlisted
    } else if any_to.chain(any_cc).any(|r| r.ends_with("/followers")) {
        Visibility::Followers
    } else {
        Visibility::Direct
    }
}

/// `Note.id` のホストが `signer` のホストと一致することを確認する。
/// `reaction::process_inbound_reaction` の `ensure_same_host` と同種の防御。
fn same_host(other_uri: &str, signer_ap_id: &str, kind: &str) -> anyhow::Result<()> {
    let other = Url::parse(other_uri)
        .with_context(|| format!("{kind} {other_uri:?} is not a valid URL"))?;
    let signer = Url::parse(signer_ap_id)
        .with_context(|| format!("signer ap_id {signer_ap_id:?} is not a valid URL"))?;
    let other_host = other.host_str().unwrap_or_default();
    let signer_host = signer.host_str().unwrap_or_default();
    if !other_host.eq_ignore_ascii_case(signer_host) {
        return Err(anyhow!(
            "{kind} host {other_host:?} does not match signer host {signer_host:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visibility_public_to_strict() {
        let v = derive_visibility(&[PUBLIC_URI.into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Public);
    }

    #[test]
    fn visibility_unlisted_when_public_in_cc() {
        let v = derive_visibility(
            &["https://x.test/users/me".into()],
            &[PUBLIC_URI.into()],
            &[],
            &[],
        );
        assert_eq!(v, Visibility::Unlisted);
    }

    #[test]
    fn visibility_followers_when_only_followers_url() {
        let v = derive_visibility(&["https://x.test/users/a/followers".into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Followers);
    }

    #[test]
    fn visibility_direct_when_only_user_uri() {
        let v = derive_visibility(&["https://x.test/users/me".into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Direct);
    }

    #[test]
    fn extract_string_array_filters_non_strings() {
        let v = json!(["a", 42, {"x": 1}, "b"]);
        assert_eq!(extract_string_array(Some(&v)), vec!["a", "b"]);
    }

    #[test]
    fn extract_string_array_handles_missing() {
        assert!(extract_string_array(None).is_empty());
        assert!(extract_string_array(Some(&JsonValue::Null)).is_empty());
    }

    #[test]
    fn extract_url_handles_string_object_array() {
        assert_eq!(
            extract_url_string(&json!("https://x")),
            Some("https://x".into())
        );
        assert_eq!(
            extract_url_string(&json!({"type": "Link", "href": "https://y"})),
            Some("https://y".into())
        );
        assert_eq!(
            extract_url_string(&json!([{"href": "https://z"}, "ignored"])),
            Some("https://z".into())
        );
    }

    #[test]
    fn same_host_accepts_matching() {
        same_host(
            "https://x.test/notes/1",
            "https://x.test/users/a",
            "Note id",
        )
        .unwrap();
    }

    #[test]
    fn same_host_rejects_different_host() {
        assert!(
            same_host(
                "https://evil.test/notes/1",
                "https://x.test/users/a",
                "Note id",
            )
            .is_err()
        );
    }
}
