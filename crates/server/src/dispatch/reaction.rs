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
//! - `object` (= 対象 Note URI) は **既に DB にある note** を指していなければ
//!   silently skip (= 連合相手の retry ループに乗らないよう 202)。local / remote
//!   は問わない ── フォロイー投稿やフォロイーの boost 経由で取り込んだ remote
//!   note (= タイムラインに並ぶ投稿) への第三者リアクションもカウント反映する
//!   (Misskey 準拠)。未知 note は fetch せず skip する (reaction 受信を契機に
//!   外部 fetch を走らせない)。in-app / webhook 通知は our own (local) note への
//!   reaction のみ発火し、remote note への reaction はカウントのみ反映する。
//! - `Emoji.id` の host は signer host と一致することを要求する ── 他インスタンス
//!   の絵文字 ID を spoofing 学習させない (identity 境界)。
//! - `Emoji.icon.url` の host は signer と異なってよい (Issue #239: Misskey は
//!   drive/画像を別ドメインで配信する)。画像取得は media-proxy が SSRF 境界を担い、
//!   client へは自鯖キャッシュ URL を返すため任意 host を許容できる。

use anyhow::Context;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{info, warn};

use super::DispatchError;
use crate::emoji_learn::{self, LearnedEmoji, extract_shortcode};
use crate::notification;
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

    // Misskey 互換 `/streaming` の noteUpdated (unreacted) へ push。削除した行の
    // note_id / content を使う (= `row` は削除前に引いてある)。
    if deleted > 0 {
        let _ = state
            .stream_sender()
            .send(crate::event_bus::StreamEvent::ReactionUpdated {
                note_id: row.note_id,
                reaction: row.content.clone(),
                kind: crate::event_bus::ReactionKind::Unreacted,
            });
    }

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
    let raw_content = extract_content(activity, kind)?;
    // Issue #186: inbound 側でも `:foo@host:` の `@host` suffix を剥がし、
    // DB / wire 上の reaction key を OUTBOUND (PR #183) と同じ `:foo:` 形に揃える。
    // 連合相手の wire form が `:foo:` だったり `:foo@theirhost:` だったり
    // `:foo@oursakurasato:` だったりするのを **shortcode 単位で 1 つのバケツに
    // まとめる** ── これで Aria など Misskey-compat client の `notes/show`
    // 表示で「同じ shortcode が 2 行に分かれて出る」現象を抑える。
    let content = normalize_inbound_reaction_content(&raw_content);

    // 対象 Note は **既に DB にある** ものだけ受ける (local / remote 問わず)。
    // フォロイー投稿・フォロイーの boost 経由で取り込んだ remote note (= タイム
    // ラインに並ぶ投稿) への第三者リアクションもカウント反映する (Misskey 準拠)。
    // DB に無い note は fetch せず skip する ── reaction 受信を契機に無制限な
    // 外部 fetch を走らせないため (= 「タイムラインにある投稿だけ」に閉じる)。
    let Some(note) = repo::note::get_by_ap_id(state.pool(), &object_uri)
        .await
        .with_context(|| format!("lookup note {object_uri}"))
        .map_err(DispatchError::Internal)?
    else {
        info!(
            kind = kind.as_str(),
            object = %object_uri,
            signer = %signer.ap_id,
            "reaction target note not stored; ignoring (silent 202, no fetch)"
        );
        return Ok(());
    };

    // `tag: [Emoji]` を学習し、対応する emoji_id があれば reaction に紐付ける。
    // [`learn_emoji_tag`] は内部で `extract_shortcode` を呼ぶので `content` /
    // `raw_content` のどちらを渡しても shortcode 比較は同じ結果になるが、
    // normalize 後の content を渡しておく方が「DB に書く値で学習する」一貫性
    // が取れる。
    let learned = learn_emoji_tag(state, signer, activity, &content).await;
    let emoji_id = learned.as_ref().map(|l| l.id);

    // Issue #242: リモート custom emoji は wire/DB content を `:shortcode@host:` 形で
    // 保つ。詳細は [`reaction_content_for_storage`] の doc を参照。
    let content = reaction_content_for_storage(content, learned.as_ref());

    let (inserted, is_new) = repo::reaction::insert_or_get(
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
        raw_content = %raw_content,
        content = %content,
        emoji_id = ?emoji_id,
        is_new,
        "reaction recorded",
    );

    // 冪等再受信 (= 既存 reaction 行を返した再配送) では通知 / streaming を
    // 発火しない ── 同一 Like の二重投函で通知フィードに二重エントリが積まれる
    // のを防ぐ (reaction の DB カウントは insert_or_get の UNIQUE で 1 のまま)。
    // 従来は「既存行への再配送でも必ず streaming を push」していたが、SSE 再接続
    // 時はクライアントが state を再 fetch する前提なので発火条件を「新規のみ」に
    // 揃えるのは意図的仕様変更 (Aria 側は reaction map の再描画で吸収していた)。
    if !is_new {
        info!(
            kind = kind.as_str(),
            reaction_id = inserted.id,
            note_id = note.id,
            signer = %signer.ap_id,
            content = %content,
            "re-delivery of an already-recorded reaction; skipping notify/streaming"
        );
        return Ok(());
    }

    // 通知発火 (fire-and-forget)。通知は **our own (local) note** への reaction
    // のみ ── remote note (= followee 等のタイムライン投稿) への第三者リアク
    // ションはカウント反映だけ行い、in-app / webhook 通知は出さない (Misskey も
    // 自分以外の note の reaction では通知しない。見知らぬ第三者のリアクション
    // で通知が洪水になるのを防ぐ)。通知本文は正規化済 content (= UI 表示と一致
    // する文字列) を使う。
    if note.is_local {
        notification::dispatch::notify_reaction(state, signer, &note, &content).await;
    }

    // Misskey 互換 `/streaming` の noteUpdated へ push (fire-and-forget)。通知と
    // 違い local / remote を問わず、タイムラインに並ぶ note の reaction 増減を
    // 購読中クライアントに反映する。
    let _ = state
        .stream_sender()
        .send(crate::event_bus::StreamEvent::ReactionUpdated {
            note_id: note.id,
            reaction: content.clone(),
            kind: crate::event_bus::ReactionKind::Reacted,
        });

    Ok(())
}

/// Issue #186: inbound reaction の content を host を剥がした `:shortcode:` 形に
/// **一旦** 正規化する。
///
/// PR #183 の OUTBOUND 側 [`parse_local_emoji_shortcode`](crate::local_api::reactions)
/// と対称形。**`@host` の host が何であろうと無条件で剥がす** ── 我々の
/// サーバが同時に複数 hostname (公開 AP host / Tailscale tailnet host 等) で
/// 見える運用 ([[deployment-tailscale-cloudflared]]) で、相手が hint してきた
/// host が「本当のリモートホスト」か「我々を指す別名」か区別する手段が
/// サーバ側に無い (= `config.server.host` との exact match だけでは tailnet
/// 経由 Aria を巻き込み拒否する)。
///
/// Unicode / 素 `:shortcode:` は touch せずそのまま返す。`:` で囲まれていない
/// 入力は Unicode 扱いで素通し。
///
/// 連合先の `tag.Emoji.name` は通常 `:shortcode:` (host suffix なし) で来るので、
/// 本関数で content から host を剥がしても [`learn_emoji_tag`] の shortcode 比較
/// は変わらず動く ── どちらも `extract_shortcode` 経由で shortcode 部だけ比較
/// しているため。
///
/// **Issue #242**: ただしこの host 剥がしは「素の正規化」で終わりではない。
/// [`process_inbound_reaction`] は本関数の出力で `learn_emoji_tag` を回し、それが
/// remote 絵文字 ([`LearnedEmoji`]) を返した場合は **`:shortcode@host:` に host を
/// 付け直して** DB / wire に書く。host 無しの `:foo:` を Misskey/Aria に渡すと
/// 「自鯖ローカル絵文字」と誤認され reactionEmojis を見ずに自鯖 store を引いてしまう
/// ため、真リモート絵文字は host を保つ必要がある。本関数が一律 strip するのは、
/// 「ローカル絵文字の往復 (#182)」と「真リモート」を文字列だけでは判別できないから
/// で、判別は emoji 学習結果 (= `is_local`) に委ねる設計。
fn normalize_inbound_reaction_content(content: &str) -> String {
    let Some(shortcode) = extract_shortcode(content) else {
        // Unicode (= `:` 囲みでない) や empty / malformed は素通し。
        // empty は呼び出し元 ([`extract_content`]) が `EmojiReact` でだけ
        // ガード済 (`Like` は空 OK)。
        return content.to_string();
    };
    format!(":{shortcode}:")
}

/// Issue #242: DB / wire に書く最終的な reaction content を決める。
///
/// `normalized` は [`normalize_inbound_reaction_content`] が host を剥がした
/// `:shortcode:` (または Unicode) 形。`learned` は [`learn_emoji_tag`] の結果:
///
/// - `Some(remote 絵文字)` → `:shortcode@host:` に host を **付け直す**。host 無しの
///   `:foo:` を渡すと Misskey/Aria が「自鯖ローカル絵文字」と誤認して reactionEmojis
///   を見ず自鯖 emoji store を引き、ローカルに同名 shortcode が無い絵文字を描画
///   できなくなる (= versitygw にキャッシュ画像はあるのに見えない)。`learn_emoji_tag`
///   は `Emoji.id` host == signer host を強制するので、学習できた絵文字は必ず
///   signer 鯖の remote 絵文字 (`is_local=false`)。
/// - `None` (Unicode / ローカル絵文字往復 #182 / 学習不能) → `normalized` のまま。
///   ローカル絵文字は host 無し `:foo:` を Aria が自鯖 store で解決するのが正しい。
fn reaction_content_for_storage(normalized: String, learned: Option<&LearnedEmoji>) -> String {
    let Some(LearnedEmoji { host, .. }) = learned else {
        return normalized;
    };
    match extract_shortcode(&normalized) {
        Some(shortcode) => format!(":{shortcode}@{host}:"),
        // learned が Some なら normalized は必ず `:foo:` 形だが、防御的に素通し。
        None => normalized,
    }
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
/// `content` に該当する `Emoji.name` が見つかればその id と host を [`LearnedEmoji`]
/// で返す。複数 tag があっても content と name (`:foo:`) が一致するものだけ採用する
/// (Note 本文の全件学習は [`crate::emoji_learn::learn_note_emoji_tags`] が別途担う)。
/// `Emoji.id` の host が signer host と異なる場合は無視する (spoofing 防止)。
/// `Emoji.icon.url` の host は signer と異なってよい (Issue #239、別ドメイン drive 対応)。
///
/// 学習自体は best-effort。失敗しても reaction の記録は続行する。1 個の tag の
/// 検証・fetch・DB upsert は [`emoji_learn::learn_emoji_tag_object`] に委譲する
/// (Note 用の全件学習と共有するために切り出された共通ロジック)。shortcode が
/// 一致した tag の学習が失敗 (host 不一致等) しても、同名 shortcode の別 tag が
/// あれば引き続き探す (= 旧実装からの挙動を保つ)。
async fn learn_emoji_tag(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
    content: &str,
) -> Option<LearnedEmoji> {
    let tags = activity.get("tag")?.as_array()?;
    let signer_host = emoji_learn::host_from_ap_id(&signer.ap_id)?;
    let content_shortcode = extract_shortcode(content)?;

    for tag in tags {
        if !emoji_learn::is_emoji_tag(tag) {
            continue;
        }
        let Some(name) = tag.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        // content と name の対応:
        //   content == ":blob:" → name == ":blob:" であること
        //   content == ":blob@misskey.io:" → name 側はホスト無しの ":blob:"
        // どちらも `name` 側は ":blob:" 形式なので、両方の表記から shortcode
        // を抽出して name と比較する。
        let Some(name_shortcode) = extract_shortcode(name) else {
            continue;
        };
        if content_shortcode != name_shortcode {
            continue;
        }
        if let Some(learned) = emoji_learn::learn_emoji_tag_object(state, &signer_host, tag).await {
            return Some(learned);
        }
        // 学習失敗 (charset不正/host不一致/icon.url不正/upsert失敗) は
        // 同名 shortcode の別 tag があるかもしれないので次の候補へ。
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Issue #186: 素の `:foo:` (= 標準的な Misskey/Mastodon wire) は touch せず
    /// そのまま返す。`build_reactions` の `BTreeMap` key として既存パスを壊さない
    /// ことを担保する回帰テスト。
    #[test]
    fn normalize_inbound_passes_through_bare_shortcode() {
        assert_eq!(normalize_inbound_reaction_content(":blob:"), ":blob:");
        assert_eq!(
            normalize_inbound_reaction_content(":blob_party:"),
            ":blob_party:"
        );
    }

    /// Issue #186 のメイン: `:foo@<anyhost>:` 形式は `:foo:` に正規化する。
    /// host が「真リモート」「自ホスト」「`.` (= local sentinel)」のいずれでも
    /// 同じく strip ── サーバ側で「自ホスト集合の正確な enumeration」が
    /// 取れない (= tailnet / 公開 AP / cloudflared 等で複数 hostname) ため、
    /// PR #183 OUTBOUND と同じく無条件 strip にする。
    #[test]
    fn normalize_inbound_strips_host_suffix_unconditionally() {
        assert_eq!(
            normalize_inbound_reaction_content(":blob@misskey.io:"),
            ":blob:"
        );
        assert_eq!(
            normalize_inbound_reaction_content(":blob@oursakurasato.test:"),
            ":blob:"
        );
        assert_eq!(normalize_inbound_reaction_content(":blob@.:"), ":blob:");
        // Misskey-dart 系の port 付き host (例: tailnet `:8443`) も `extract_shortcode`
        // が `split('@').next()` で先頭 `:foo` 部分だけ拾うので OK。
        assert_eq!(
            normalize_inbound_reaction_content(":blob@foo.tailnet.ts.net:8443:"),
            ":blob:"
        );
    }

    /// Issue #186: Unicode リアクション (= `:` 囲みでない裸文字列) は touch せず
    /// 素通し。`Like` activity の空 content も同じく素通し。
    #[test]
    fn normalize_inbound_passes_through_unicode_and_empty() {
        assert_eq!(normalize_inbound_reaction_content("👍"), "👍");
        assert_eq!(normalize_inbound_reaction_content("❤"), "❤");
        // 空文字 ── `Like` (= Mastodon の favourite) で来る wire 形式。
        assert_eq!(normalize_inbound_reaction_content(""), "");
        // 片側 colon ── `extract_shortcode` が `None` を返すので素通し。
        // downstream で `(note_id, actor_id, content)` UNIQUE を踏まないよう
        // にしているのは insert_or_get 側責務。
        assert_eq!(normalize_inbound_reaction_content(":blob"), ":blob");
        assert_eq!(normalize_inbound_reaction_content("blob:"), "blob:");
    }

    /// Issue #186: `:@host:` (= shortcode 部が空) は不正形式として touch しない。
    /// `extract_shortcode` の既存挙動 (= `None` を返す) と整合し、`learn_emoji_tag`
    /// もこの content では emoji を学習しない。
    #[test]
    fn normalize_inbound_does_not_touch_malformed_empty_shortcode() {
        assert_eq!(normalize_inbound_reaction_content(":@host:"), ":@host:");
        assert_eq!(normalize_inbound_reaction_content("::"), "::");
    }

    /// Issue #242 のメイン: remote 絵文字を学習できた reaction は、host を剥がした
    /// `:foo:` ではなく `:foo@host:` を DB / wire に書く。Misskey/Aria が
    /// reactionEmojis 経由で解決できるようにするため。
    #[test]
    fn reaction_content_reattaches_host_for_learned_remote_emoji() {
        let learned = LearnedEmoji {
            id: 42,
            host: "misskey.io".into(),
        };
        assert_eq!(
            reaction_content_for_storage(":blob:".into(), Some(&learned)),
            ":blob@misskey.io:"
        );
        // normalize 前に host が付いていても、normalize 済 `:foo:` を渡す前提なので
        // learn 側の host が勝つ (= signer host で一貫させる)。
        let learned2 = LearnedEmoji {
            id: 7,
            host: "example.test".into(),
        };
        assert_eq!(
            reaction_content_for_storage(":blob_party:".into(), Some(&learned2)),
            ":blob_party@example.test:"
        );
    }

    /// Issue #242: 学習できなかった (= `None`) reaction は normalize 済 content を
    /// そのまま使う。Unicode / ローカル絵文字往復 (#182) / 学習不能はここに合流し、
    /// host 無し `:foo:` のまま (= Aria が自鯖 store で解決するのが正しい)。
    #[test]
    fn reaction_content_passthrough_when_not_learned() {
        assert_eq!(
            reaction_content_for_storage(":blob:".into(), None),
            ":blob:"
        );
        assert_eq!(reaction_content_for_storage("👍".into(), None), "👍");
        assert_eq!(reaction_content_for_storage(String::new(), None), "");
    }

    /// Issue #242: Unicode を learned 扱いで渡す経路は実際には起きない
    /// (`learn_emoji_tag` は `:foo:` 形でしか shortcode 一致しない) が、防御的に
    /// `extract_shortcode` が `None` を返す content は素通しすることを固定する。
    #[test]
    fn reaction_content_defensive_passthrough_for_non_shortcode() {
        let learned = LearnedEmoji {
            id: 1,
            host: "misskey.io".into(),
        };
        assert_eq!(
            reaction_content_for_storage("👍".into(), Some(&learned)),
            "👍"
        );
    }
}
