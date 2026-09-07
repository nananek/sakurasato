//! 受領 `Announce` (Boost) ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - 受け入れ条件: signer が我々の **followee** である (= local actor が
//!   signer を `accepted` で follow している)。followee 以外の boost は
//!   debug ログのみで no-op ── 他人の boost で見知らぬ note を引き込まない。
//! - 対象 Note が既知 (= こちらの DB にある) ならそのまま、未知なら
//!   **origin から fetch して取り込む** (Issue #266、下記)。
//! - boost は `announce` テーブルに idempotent insert。`Undo Announce` は
//!   対称的に `delete_by_ap_id` で消す (本ファイル内で `handle_undo_announce`
//!   を提供し、`super::dispatch_undo` から呼ばれる)。
//!
//! ## 未知 Note の fetch (Issue #266)
//!
//! M11 (#55) では「Announce 受信時の未知 Note 自動 fetch」をスコープ外にして
//! いたが、フォローしていない著者の投稿への被リノートが「DB 未取得 = 未知」で
//! 表示されない問題につながった。Mastodon / Misskey と同様、**followee の
//! Announce に限って** `object` URI を `GET` して取り込む
//! ([`super::note::fetch_and_store_remote_note`])。fetch は actor fetch と同じ
//! SSRF / redirect / サイズガードを共有し、画像デコードを伴わない JSON GET な
//! ので CLAUDE.md §3 の server 直 fetch 例外に収まる。fetch 失敗は boost を
//! 黙って捨てる (従来の no-op と同じ着地)。

use anyhow::Context;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{debug, info, warn};

use super::DispatchError;
use crate::notification;
use crate::state::AppState;

pub(crate) async fn handle_announce(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    let activity_id = super::extract_activity_id(activity)?.to_string();
    let target_uri = super::extract_object_uri(activity)?.to_string();

    // followee gate. お一人様 follow グラフ自体が小さいので毎回 LIST して
    // 空判定で十分。
    let followed = !repo::follow::list_local_following(state.pool(), signer.id)
        .await
        .context("list_local_following for Announce filter")
        .map_err(DispatchError::Internal)?
        .is_empty();
    if !followed {
        debug!(
            target = %target_uri,
            signer = %signer.ap_id,
            "Announce: signer is not a followee; ignoring",
        );
        return Ok(());
    }

    let note = match repo::note::get_by_ap_id(state.pool(), &target_uri)
        .await
        .with_context(|| format!("lookup note {target_uri}"))
        .map_err(DispatchError::Internal)?
    {
        Some(note) => note,
        None => {
            // **Issue #266**: M11 ではスコープ外にしていた「未知 Note の fetch」
            // を followee の Announce に限り許可する ── これをしないと、
            // フォローしていないアカウントの投稿への被リノートが
            // 「DB 未取得 = 未知」で表示されない (報告バグ)。fetch 失敗
            // (SSRF ガード / 404 / cross-origin / 非公開 / malformed) は従来
            // どおり boost を黙って捨てて 202 を返す。
            match super::note::fetch_and_store_remote_note(state, &target_uri).await {
                Ok(note) => note,
                Err(err) => {
                    debug!(
                        target = %target_uri,
                        signer = %signer.ap_id,
                        ?err,
                        "Announce: target note unknown and fetch failed; dropping boost",
                    );
                    return Ok(());
                }
            }
        }
    };

    let published_at = super::parse_ap_timestamp(activity.get("published"));

    let row =
        repo::announce::insert_or_get(state.pool(), &activity_id, note.id, signer.id, published_at)
            .await
            .with_context(|| format!("insert announce {activity_id}"))
            .map_err(DispatchError::Internal)?;

    info!(
        announce_id = row.id,
        note_id = note.id,
        signer = %signer.ap_id,
        "boost recorded",
    );

    // Misskey 互換 `/streaming` の homeTimeline へ renote frame として push
    // (fire-and-forget)。通知 (下の notify_renote) は自分の note の boost のみ
    // だが、home timeline には followee の boost を is_local を問わず並べる
    // (= REST の list_home_renote_window と対称)。
    let _ = state
        .stream_sender()
        .send(crate::event_bus::StreamEvent::Renote {
            announce_id: row.id,
        });

    // 通知は **自分の note が boost された時だけ** 発火する (Misskey / Mastodon
    // と同じ作法)。followee が第三者の note を boost したのは home timeline の
    // 内容であって「自分への通知」ではない ── これを通知に流すと Aria の通知
    // タブが他人同士の renote で埋まる (報告バグ)。お一人様 server では
    // `note.is_local` が「自分 (local actor) の note」と同値。
    if note.is_local {
        notification::dispatch::notify_renote(state, signer, &note).await;
    } else {
        debug!(
            note_id = note.id,
            signer = %signer.ap_id,
            "Announce: boosted note is not ours; recorded but not notified",
        );
    }

    Ok(())
}

/// Undo Announce 用エントリ。`super::dispatch_undo` から `object.type` が
/// `Announce` のときに呼ばれる。`object` を URI として扱い、`announce`
/// テーブルから対応行を削除する。
///
/// 第三者 Undo (= 別 actor が他人の Announce を消す) を防ぐため、削除前に
/// `actor_id == signer.id` を検査する。Undo reaction と同パターン。
pub(crate) async fn handle_undo_announce(
    state: &AppState,
    signer: &ActorRow,
    target_ap_id: &str,
) -> Result<(), DispatchError> {
    let row = repo::announce::get_by_ap_id(state.pool(), target_ap_id)
        .await
        .with_context(|| format!("lookup announce {target_ap_id}"))
        .map_err(DispatchError::Internal)?;

    let Some(row) = row else {
        debug!(
            target = target_ap_id,
            signer = %signer.ap_id,
            "Undo Announce: unknown announce; ignoring (we never received the original)",
        );
        return Ok(());
    };

    if row.actor_id != signer.id {
        warn!(
            target = target_ap_id,
            signer = %signer.ap_id,
            announce_actor = row.actor_id,
            "Undo Announce: signer is not the boost's actor; refusing",
        );
        return Err(DispatchError::Malformed(
            "Undo Announce signer does not match boost actor".into(),
        ));
    }

    let deleted = repo::announce::delete_by_ap_id(state.pool(), target_ap_id)
        .await
        .with_context(|| format!("delete announce {target_ap_id}"))
        .map_err(DispatchError::Internal)?;
    info!(
        target = target_ap_id,
        signer = %signer.ap_id,
        deleted,
        "boost undone",
    );
    Ok(())
}
