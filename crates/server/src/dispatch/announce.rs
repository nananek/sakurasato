//! 受領 `Announce` (Boost) ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - 対象: 既知 (= こちらの DB にある) ローカル / リモート Note の boost のみ。
//! - 受け入れ条件: signer が我々の **followee** である (= local actor が
//!   signer を `accepted` で follow している)。followee 以外の boost は
//!   debug ログのみで no-op ── 他人の boost で見知らぬ note を引き込まない。
//! - 既知 Note への boost は `announce` テーブルに idempotent insert。
//!   `Undo Announce` は対称的に `delete_by_ap_id` で消す (本ファイル内で
//!   `handle_undo_announce` を提供し、`super::dispatch_undo` から呼ばれる)。
//!
//! ## 未対応 (将来作業)
//!
//! - **未知 Note の自動 fetch**: nekonoverse / Mastodon は Announce 受信時に
//!   `object` URI を `GET` して取り込むのが慣習だが、対応すると外向き fetch
//!   経路を増やすことになる (今は CLAUDE.md §3 で remote actor fetch 経路
//!   だけが server 直接 GET を持つ)。本 M11 ではスコープ外とし、未知 Note
//!   の Announce は debug ログのみで捨てる。

use anyhow::Context;
use chrono::{DateTime, Utc};
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

    let Some(note) = repo::note::get_by_ap_id(state.pool(), &target_uri)
        .await
        .with_context(|| format!("lookup note {target_uri}"))
        .map_err(DispatchError::Internal)?
    else {
        debug!(
            target = %target_uri,
            signer = %signer.ap_id,
            "Announce: target note unknown; no fetch (M11 scope)",
        );
        return Ok(());
    };

    let published_at = activity
        .get("published")
        .and_then(JsonValue::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc));

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

    // 通知発火 (fire-and-forget)。Note は既知のもの (= 我々 local もしくは
    // 我々が引き込んだ remote note) なので、boost が自分の note でなくても
    // 「フォロー先が誰かの何かを boost した」が webhook に流れる。煩ければ
    // `notify_renote` 列で off にする運用。
    notification::dispatch::notify_renote(state, signer, &note).await;

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
