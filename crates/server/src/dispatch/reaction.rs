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
use aws_sdk_s3::primitives::ByteStream;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{info, warn};
use url::Url;

use super::DispatchError;
use crate::notification;
use crate::state::AppState;

/// Issue #135: remote emoji を media-proxy 経由で取得してキャッシュする
/// ときの variant 文字列 (= `emoji_import.rs::EMOJI_VARIANT` と同値)。
/// media-proxy 側の `Variant::Emoji` (512x512 / WebP 単一フレーム or animated)
/// と揃える ── `crates/media-proxy/src/image_pipeline.rs` を参照。
const EMOJI_VARIANT: &str = "emoji";

/// Issue #135: 取得済み remote emoji を versitygw に置く prefix。
/// `routes/media.rs` の許可リスト ([`crate::routes::media::REMOTE_EMOJI_KEY_PREFIX`])
/// と同期させる ── 名前を grep で揃えやすいよう同 prefix を使う。
const REMOTE_EMOJI_KEY_PREFIX: &str = "emoji/remote/";

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
    let raw_content = extract_content(activity, kind)?;
    // Issue #186: inbound 側でも `:foo@host:` の `@host` suffix を剥がし、
    // DB / wire 上の reaction key を OUTBOUND (PR #183) と同じ `:foo:` 形に揃える。
    // 連合相手の wire form が `:foo:` だったり `:foo@theirhost:` だったり
    // `:foo@oursakurasato:` だったりするのを **shortcode 単位で 1 つのバケツに
    // まとめる** ── これで Aria など Misskey-compat client の `notes/show`
    // 表示で「同じ shortcode が 2 行に分かれて出る」現象を抑える。
    let content = normalize_inbound_reaction_content(&raw_content);

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
    // [`learn_emoji_tag`] は内部で `extract_shortcode` を呼ぶので `content` /
    // `raw_content` のどちらを渡しても shortcode 比較は同じ結果になるが、
    // normalize 後の content を渡しておく方が「DB に書く値で学習する」一貫性
    // が取れる。
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
        raw_content = %raw_content,
        content = %content,
        emoji_id = ?emoji_id,
        "reaction recorded",
    );

    // 通知発火 (fire-and-forget)。reaction target は local note のみここに来る
    // (上で `is_local` チェック済み)。通知本文も正規化済 content (= UI 表示
    // と一致する文字列) を使う。
    notification::dispatch::notify_reaction(state, signer, &note, &content).await;

    Ok(())
}

/// Issue #186: inbound reaction の content を `:shortcode:` 形に正規化する。
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
fn normalize_inbound_reaction_content(content: &str) -> String {
    let Some(shortcode) = extract_shortcode(content) else {
        // Unicode (= `:` 囲みでない) や empty / malformed は素通し。
        // empty は呼び出し元 ([`extract_content`]) が `EmojiReact` でだけ
        // ガード済 (`Like` は空 OK)。
        return content.to_string();
    };
    format!(":{shortcode}:")
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
#[allow(
    clippy::too_many_lines,
    reason = "single tag-loop over Activity.tag[]; Issue #135 で cache/fetch 分岐が増えただけで構造は線形"
)]
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
        // PR #191 round-1 ⚠️ #1: `extract_shortcode` は `:` を剥がして `@` で
        // 分割するだけで charset を見ない。`/` 入りの malicious shortcode が
        // versitygw に `emoji/remote/<host>/a/b.webp` で書かれて namespace を
        // 汚染しないよう、fetch + PUT の前で `is_valid_shortcode` を強制する。
        // `upsert_remote` 内の `is_valid_shortcode` 検査は upsert 前に失敗する
        // が、その時点では既に versitygw に PUT 済 ── 早期に弾く必要がある。
        if !repo::emoji::is_valid_shortcode(shortcode) {
            warn!(
                shortcode,
                signer = %signer.ap_id,
                "Emoji.name shortcode failed charset validation; refusing to learn"
            );
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

        // Issue #135: 既存 row が新形式 (= `emoji/remote/...`) でキャッシュ済
        // ならネットワーク fetch をスキップして DB だけ touch する。Mastodon /
        // Misskey は emoji tag を per-note で送るので、同じ remote 絵文字を
        // 何度も学習する経路 ── ここを cache hit で短絡しないと毎 note 受信
        // ごとに相手サーバへ GET が飛ぶ。
        let cached_key: Option<String> = match repo::emoji::get_by_ap_id(state.pool(), ap_id).await
        {
            Ok(Some(existing)) => existing
                .image_key
                .filter(|k| k.starts_with(REMOTE_EMOJI_KEY_PREFIX)),
            Ok(None) => None,
            Err(err) => {
                warn!(?err, emoji_id = ap_id, "get_by_ap_id failed; refetching");
                None
            }
        };

        let (image_key, stored_media_type) = if let Some(key) = cached_key {
            (Some(key), media_type.to_string())
        } else {
            match fetch_and_cache_remote_emoji(state, image_url, &signer_host, shortcode).await {
                Ok((key, mt)) => (Some(key), mt),
                Err(err) => {
                    // 取得失敗は warn にとどめて upsert を続行 ── image_key=None
                    // で「画像なし、テキストフォールバック」を記録する (= AC4)。
                    warn!(
                        ?err,
                        emoji_id = ap_id,
                        image_url,
                        "remote emoji fetch/cache failed; storing without image_key"
                    );
                    (None, media_type.to_string())
                }
            }
        };

        let new = repo::emoji::NewRemoteEmoji {
            shortcode: shortcode.to_string(),
            ap_id: ap_id.to_string(),
            host: signer_host.clone(),
            image_key,
            media_type: stored_media_type,
        };
        match repo::emoji::upsert_remote(state.pool(), new).await {
            Ok(row) => {
                info!(
                    emoji_id = row.id,
                    shortcode = %shortcode,
                    host = %signer_host,
                    image_cached = row.image_key.is_some(),
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

/// Issue #135: remote emoji の画像を media-proxy 経由で取得し、versitygw に
/// 格納する。成功時は `(versitygw_key, "image/webp")` を返す。失敗時は anyhow
/// エラーを返し、呼び出し側で `image_key = None` を選ばせる。
///
/// **副作用**: versitygw 上に `emoji/remote/<host>/<shortcode>.webp` を PUT する。
/// 同名 key への複数回 PUT は idempotent (= 上書き) なので、cache hit 判定で
/// 弾けなかった経路で重複 PUT が走っても害は無い。
async fn fetch_and_cache_remote_emoji(
    state: &AppState,
    image_url: &str,
    signer_host: &str,
    shortcode: &str,
) -> anyhow::Result<(String, String)> {
    let processed = state
        .media_proxy()
        .fetch_image(image_url, EMOJI_VARIANT)
        .await
        .with_context(|| format!("media-proxy fetch {image_url}"))?;

    // host / shortcode は事前に検証済み (signer_host=正規化済 hostname、
    // shortcode=`[a-zA-Z0-9_-]{1,128}`)。path traversal にならない。
    let storage_key = format!("{REMOTE_EMOJI_KEY_PREFIX}{signer_host}/{shortcode}.webp");
    let media_type = processed.content_type.clone();
    state
        .s3_client()
        .put_object()
        .bucket(&state.config().storage.bucket)
        .key(&storage_key)
        .content_type(&media_type)
        .body(ByteStream::from(processed.bytes))
        .send()
        .await
        .with_context(|| format!("versitygw PUT {storage_key}"))?;
    Ok((storage_key, media_type))
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

    /// PR #191 round-1 ⚠️ #1: `extract_shortcode` 自身は charset を見ない。
    /// `/` 入りの shortcode を Some で返してしまうため、後段の
    /// `is_valid_shortcode` が弾く責務を持つ ── 本テストはその境界仕様を
    /// 固定する (= `extract_shortcode` の挙動を不用意に厳しくしないため)。
    #[test]
    fn extract_shortcode_does_not_filter_charset() {
        // `learn_emoji_tag` 側で `is_valid_shortcode` を必ず呼ぶ前提で、
        // ここでは「`/` を含む値も Some で返る」ことを記録する。
        assert_eq!(extract_shortcode(":a/b:"), Some("a/b"));
        // `is_valid_shortcode` 側がそれを拒否することは core 側の責務。
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode("a/b"));
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode(
            "../escape"
        ));
        assert!(!sakurasato_core::repo::emoji::is_valid_shortcode(""));
        assert!(sakurasato_core::repo::emoji::is_valid_shortcode("blob"));
        assert!(sakurasato_core::repo::emoji::is_valid_shortcode(
            "blob_party-1"
        ));
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
}
