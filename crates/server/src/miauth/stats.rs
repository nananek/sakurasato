//! `POST /api/stats` ── Misskey 互換インスタンス統計 (= #168 / 親 #150)。
//!
//! Misskey クライアント (Milktea / `MissRirica`) はインスタンス overview 画面で
//! 本 endpoint を probe して `notesCount` / `usersCount` / `instances` を表示する。
//! login flow 自体は通るが、無いと UI 上で「-」表示や Spinner 残留になる。
//!
//! ## Sakurasato での意味付け
//!
//! Misskey 仕様:
//! - `notesCount` ── サーバが知る全 Note 数 (= local + キャッシュ済 remote)
//! - `originalNotesCount` ── このサーバ所属ユーザの Note 数 (= local 限定)
//! - `usersCount` ── 全 actor (local + remote)
//! - `originalUsersCount` ── このサーバ所属 actor (= お一人様なら 1)
//! - `instances` ── 既知 remote instance 数 (= distinct host)
//! - `driveUsageLocal` / `driveUsageRemote` ── ストレージ使用量 (bytes)
//!
//! Sakurasato 実装は **お一人様前提** で:
//! - `notesCount` = `originalNotesCount` = local note 数 (= remote cache は
//!   stats に晒さない、UI 上は local 数のみで違和感ない)
//! - `originalUsersCount` = 1 固定
//! - `usersCount` ← stub (local 1 件のみ計上、remote 数は晒さない)
//! - `instances` ← `repo::actor::count_distinct_remote_hosts` で正確に集計
//! - `driveUsage*` ← 0 stub (計算コスト高 + 重要度低 + UI でも目立たない)
//!
//! ## AGPL discipline
//!
//! [api-doc.misskey.io](https://api-doc.misskey.io/) /
//! [4ster1sk/nkv-proxy](https://github.com/4ster1sk/nkv-proxy)
//! (`app/api/mk/meta.py::api_stats`) を一次資料 ── Misskey 本体 TypeScript
//! handler は未参照、`[[agpl-discipline-miauth]]` 準拠。

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;
use tracing::warn;

use crate::state::AppState;

/// `POST /api/stats` のレスポンス。Misskey 仕様の field 名 (= snake-free
/// camelCase) と整数型 (= u64) で揃える。
#[derive(Debug, Serialize)]
pub struct Stats {
    #[serde(rename = "notesCount")]
    pub notes_count: u64,
    #[serde(rename = "originalNotesCount")]
    pub original_notes_count: u64,
    #[serde(rename = "usersCount")]
    pub users_count: u64,
    #[serde(rename = "originalUsersCount")]
    pub original_users_count: u64,
    pub instances: u64,
    #[serde(rename = "driveUsageLocal")]
    pub drive_usage_local: u64,
    #[serde(rename = "driveUsageRemote")]
    pub drive_usage_remote: u64,
}

/// `POST /api/stats` handler。認証不要 (= overview 画面で login 前にも叩かれる
/// ケースがあるため。Misskey 公式も認証無しで返す)。
pub async fn handle(State(state): State<AppState>) -> Response {
    let notes = repo::note::count_local(state.pool()).await.map_or_else(
        |err| {
            warn!(?err, "stats: count_local failed; emitting 0");
            0
        },
        |n| u64::try_from(n).unwrap_or(0),
    );

    let instances = repo::actor::count_distinct_remote_hosts(state.pool())
        .await
        .map_or_else(
            |err| {
                warn!(?err, "stats: count_distinct_remote_hosts failed; emitting 0");
                0
            },
            |n| u64::try_from(n).unwrap_or(0),
        );

    let body = Stats {
        notes_count: notes,
        original_notes_count: notes,
        users_count: 1,
        original_users_count: 1,
        instances,
        drive_usage_local: 0,
        drive_usage_remote: 0,
    };
    Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn stats_serializes_to_camelcase() {
        let s = Stats {
            notes_count: 10,
            original_notes_count: 10,
            users_count: 1,
            original_users_count: 1,
            instances: 3,
            drive_usage_local: 0,
            drive_usage_remote: 0,
        };
        let v: Value = serde_json::to_value(&s).expect("serialize");
        // すべての field が camelCase で乗ること
        for key in &[
            "notesCount",
            "originalNotesCount",
            "usersCount",
            "originalUsersCount",
            "instances",
            "driveUsageLocal",
            "driveUsageRemote",
        ] {
            assert!(v.get(*key).is_some(), "missing field: {key}");
        }
        assert_eq!(v["notesCount"], 10);
        assert_eq!(v["originalNotesCount"], 10);
        assert_eq!(v["usersCount"], 1);
        assert_eq!(v["instances"], 3);
    }
}
