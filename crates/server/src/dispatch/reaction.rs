//! 受領 `Like` / `EmojiReact` / `Undo` のハンドラ (M8 PR2)。
//!
//! ## 取扱う Activity
//!
//! - **`Like`** (Mastodon): `{actor, object: <note URI>, content?: 絵文字}`。
//!   `content` 無しの Like は ❤ 相当として `reaction.content = ""` で記録。
//! - **`EmojiReact`** (Misskey 拡張): `{actor, object: <note URI>, content: ":foo:" | ":foo@host:" | Unicode}`。
//!   `content` 必須。`tag: [{type: "Emoji", id, name: ":foo:", icon: {url, mediaType}}]`
//!   で remote 絵文字メタデータを学習する。
//! - **`Undo`** (Like / `EmojiReact` 双方): `{actor, object: <original Activity URI | inline>}`。
//!   `reaction.ap_id` で削除する。
//!
//! ## 信頼境界
//!
//! - F3 (body actor == signer) と nested actor 検査は [`super::dispatch`] が
//!   通過済み。ここでは signer を「`object` の author 本人」とみなしてよい。
//! - `object` (= 対象 Note URI) はこちらの local note を指していなければ
//!   silently skip (= 連合相手の retry ループに乗らないよう 202)。
//! - `Emoji.id` の host と `Emoji.icon.url` の host は signer host と一致する
//!   ことを要求する ── 他インスタンスの絵文字 ID を spoofing 学習させない。

use anyhow::Context;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{info, warn};
use url::Url;

use super::DispatchError;
use crate::state::AppState;

/// 受領 `Like` の処理。
///
/// Mastodon は `content` を出さない (=❤️ 暗黙)。Pleroma 系派生は Unicode emoji
/// を載せることがある。`content` 無しは空文字で記録し、表示層で ♥ にフォール
/// バックする (M8 PR3 TUI 側で対応)。
pub(crate) async fn handle_like(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    process_inbound_reaction(state, signer, activity, ReactionKind::Like).await
}

/// 受領 `EmojiReact` の処理 (Misskey 拡張)。`content` は必須。
pub(crate) async fn handle_emoji_react(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    process_inbound_reaction(state, signer, activity, ReactionKind::EmojiReact).await
}

/// 受領 `Undo` の処理。`object` は元 Activity (Like / `EmojiReact` / Follow など)。
///
/// `object` から `ap_id` (URI 文字列 or `{id: ...}` の `id`) を抜き、
/// `reaction` テーブルから対応行を削除する。Follow の Undo は本 PR では未対応
/// (= 元 Activity の `type` を見て区別する分岐は将来追加)。
pub(crate) async fn handle_undo(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    // Undo の object は文字列 URI または完全オブジェクト。reaction の
    // 識別子はその `id` (= Activity URI)。
    let target_ap_id = super::extract_object_uri(activity)?.to_string();

    // 削除前に対象 reaction を引き、signer が actor 本人であることを確認する。
    // F3 (body actor == signer) で signer が Undo の actor 本人だと保証され
    // ているが、対象 reaction の actor まで一致しないと「他人の Like を Undo」
    // を許してしまう。
    let row = repo::reaction::get_by_ap_id(state.pool(), &target_ap_id)
        .await
        .with_context(|| format!("lookup reaction {target_ap_id}"))
        .map_err(DispatchError::Internal)?;

    let Some(row) = row else {
        info!(
            target = %target_ap_id,
            signer = %signer.ap_id,
            "Undo references unknown reaction; ignoring (likely an Undo for an Activity we never received)"
        );
        return Ok(());
    };

    if row.actor_id != signer.id {
        warn!(
            target = %target_ap_id,
            signer = %signer.ap_id,
            reaction_actor = row.actor_id,
            "Undo signer is not the reaction's actor; refusing"
        );
        return Err(DispatchError::Malformed(
            "Undo signer does not match reaction actor".into(),
        ));
    }

    let deleted = repo::reaction::delete_by_ap_id(state.pool(), &target_ap_id)
        .await
        .with_context(|| format!("delete reaction {target_ap_id}"))
        .map_err(DispatchError::Internal)?;
    info!(
        target = %target_ap_id,
        signer = %signer.ap_id,
        deleted,
        "reaction undone",
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum ReactionKind {
    Like,
    EmojiReact,
}

impl ReactionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Like => "Like",
            Self::EmojiReact => "EmojiReact",
        }
    }
}

/// `Like` / `EmojiReact` の共通処理。
async fn process_inbound_reaction(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
    kind: ReactionKind,
) -> Result<(), DispatchError> {
    let activity_id = super::extract_activity_id(activity)?.to_string();
    let object_uri = super::extract_object_uri(activity)?.to_string();
    let content = extract_content(activity, kind)?;

    // 対象 Note は **local** でなければ受けない (= remote 同士の reaction が
    // 我々の inbox に流れてくる経路は想定しないし、流れてきても DB に Note
    // が無いので reaction を作れない)。
    let Some(note) = repo::note::get_by_ap_id(state.pool(), &object_uri)
        .await
        .with_context(|| format!("lookup note {object_uri}"))
        .map_err(DispatchError::Internal)?
    else {
        info!(
            kind = kind.as_str(),
            object = %object_uri,
            signer = %signer.ap_id,
            "reaction target note not found locally; ignoring (silent 202)"
        );
        return Ok(());
    };
    if !note.is_local {
        info!(
            kind = kind.as_str(),
            object = %object_uri,
            signer = %signer.ap_id,
            "reaction target is a remote note; ignoring"
        );
        return Ok(());
    }

    // `tag: [Emoji]` を学習し、対応する emoji_id があれば reaction に紐付ける。
    let emoji_id = learn_emoji_tag(state, signer, activity, &content).await;

    let inserted = repo::reaction::insert_or_get(
        state.pool(),
        &activity_id,
        note.id,
        signer.id,
        &content,
        emoji_id,
    )
    .await
    .with_context(|| format!("upsert reaction {activity_id}"))
    .map_err(DispatchError::Internal)?;

    info!(
        kind = kind.as_str(),
        reaction_id = inserted.id,
        note_id = note.id,
        signer = %signer.ap_id,
        content = %content,
        emoji_id = ?emoji_id,
        "reaction recorded",
    );
    Ok(())
}

/// `content` を Activity から取り出す。
///
/// `EmojiReact` は必須。`Like` は欠如可 (= 空文字 = ❤ 相当)。
fn extract_content(activity: &JsonValue, kind: ReactionKind) -> Result<String, DispatchError> {
    let c = activity
        .get("content")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    if c.is_empty() && matches!(kind, ReactionKind::EmojiReact) {
        return Err(DispatchError::Malformed(
            "EmojiReact requires a non-empty content".into(),
        ));
    }
    // 長すぎる content は受けない (Misskey の実値は最大 100 文字程度)。連合
    // 相手から `aaaa...` を流し込まれて DB を膨らませない。
    if c.chars().count() > 256 {
        return Err(DispatchError::Malformed(
            "reaction content exceeds 256-character limit".into(),
        ));
    }
    Ok(c)
}

/// Activity の `tag: [Emoji]` から remote 絵文字を学習する。
///
/// `content` に該当する `Emoji.name` が見つかればその id を返す。複数 tag が
/// あっても content と name (`:foo:`) が一致するものだけ採用する。signer の
/// host と異なる host の Emoji は無視する (spoofing 防止)。
///
/// 学習自体は best-effort。失敗しても reaction の記録は続行する。
async fn learn_emoji_tag(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
    content: &str,
) -> Option<i64> {
    let tags = activity.get("tag")?.as_array()?;
    let signer_host = Url::parse(&signer.ap_id).ok()?.host_str()?.to_lowercase();

    for tag in tags {
        let Some(obj) = tag.as_object() else {
            continue;
        };
        if obj
            .get("type")
            .and_then(JsonValue::as_str)
            .is_none_or(|t| !t.eq_ignore_ascii_case("Emoji"))
        {
            continue;
        }
        let Some(name) = obj.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        // content と name の対応:
        //   content == ":blob:" → name == ":blob:" であること
        //   content == ":blob@misskey.io:" → name 側はホスト無しの ":blob:"
        // どちらも `name` 側は ":blob:" 形式なので、両方の表記から shortcode
        // を抽出して name と比較する。
        let shortcode = extract_shortcode(content)?;
        let Some(name_shortcode) = extract_shortcode(name) else {
            continue;
        };
        if shortcode != name_shortcode {
            continue;
        }

        let Some(ap_id) = obj.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        // Emoji.id の host が signer host と一致しなければ拒否 (spoofing)。
        let Ok(emoji_url) = Url::parse(ap_id) else {
            continue;
        };
        let Some(emoji_host) = emoji_url.host_str() else {
            continue;
        };
        if !emoji_host.eq_ignore_ascii_case(&signer_host) {
            warn!(
                emoji_id = ap_id,
                signer_host = %signer_host,
                emoji_host,
                "Emoji.id host mismatch with signer; refusing to learn"
            );
            continue;
        }

        let icon = obj.get("icon").and_then(JsonValue::as_object);
        let image_url = icon
            .and_then(|i| i.get("url"))
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        let media_type = icon
            .and_then(|i| i.get("mediaType"))
            .and_then(JsonValue::as_str)
            .unwrap_or("application/octet-stream");

        if image_url.is_empty() {
            continue;
        }
        // image_url の host も signer host と一致することを要求する。Misskey の
        // 実装では media.misskey.io 等の CDN ホストを使う場合があり、本検査を
        // 厳格にしすぎると正常データを弾く ── 本 PR では同 host 一致のみを
        // 許容し、CDN 経由は M9 で再評価する。
        let url_ok = Url::parse(image_url).is_ok_and(|u| {
            u.host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&signer_host))
        });
        if !url_ok {
            warn!(
                emoji_id = ap_id,
                signer_host = %signer_host,
                image_url,
                "Emoji.icon.url host mismatch with signer; refusing to learn"
            );
            continue;
        }

        let new = repo::emoji::NewRemoteEmoji {
            shortcode: shortcode.to_string(),
            ap_id: ap_id.to_string(),
            host: signer_host.clone(),
            image_url: image_url.to_string(),
            media_type: media_type.to_string(),
        };
        match repo::emoji::upsert_remote(state.pool(), new).await {
            Ok(row) => {
                info!(
                    emoji_id = row.id,
                    shortcode = %shortcode,
                    host = %signer_host,
                    "remote emoji learned",
                );
                return Some(row.id);
            }
            Err(err) => {
                warn!(
                    ?err,
                    emoji_id = ap_id,
                    "remote emoji upsert failed; reaction will be stored without emoji_id"
                );
                return None;
            }
        }
    }
    None
}

/// `:foo:` / `:foo@host:` の `foo` 部分だけを返す。`:` で挟まれていない場合は
/// `None` (= Unicode emoji)。
fn extract_shortcode(content: &str) -> Option<&str> {
    let stripped = content.strip_prefix(':')?.strip_suffix(':')?;
    // `:foo@host:` 形式から host を落とす。
    let shortcode = stripped.split('@').next()?;
    if shortcode.is_empty() {
        None
    } else {
        Some(shortcode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_shortcode_strips_colons_and_host() {
        assert_eq!(extract_shortcode(":blob:"), Some("blob"));
        assert_eq!(extract_shortcode(":blob_party:"), Some("blob_party"));
        assert_eq!(extract_shortcode(":blob@misskey.io:"), Some("blob"));
        assert_eq!(extract_shortcode("👍"), None);
        assert_eq!(extract_shortcode(""), None);
        assert_eq!(extract_shortcode(":"), None);
        assert_eq!(extract_shortcode("::"), None);
        assert_eq!(extract_shortcode(":@host:"), None);
    }

    #[test]
    fn extract_content_required_for_emoji_react() {
        let activity = serde_json::json!({"type": "EmojiReact"});
        let err = extract_content(&activity, ReactionKind::EmojiReact).unwrap_err();
        assert!(matches!(err, DispatchError::Malformed(_)));

        // Like は空 OK (= ❤)。
        let like = serde_json::json!({"type": "Like"});
        let c = extract_content(&like, ReactionKind::Like).unwrap();
        assert_eq!(c, "");
    }

    #[test]
    fn extract_content_caps_length() {
        let big = "a".repeat(257);
        let activity = serde_json::json!({"type": "EmojiReact", "content": big});
        let err = extract_content(&activity, ReactionKind::EmojiReact).unwrap_err();
        assert!(matches!(err, DispatchError::Malformed(_)));
    }
}
