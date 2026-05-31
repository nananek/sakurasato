//! `move-accept` CLI (M10) — 保存済み `Move` activity 本文を CLI で再処理する。
//!
//! # 役割
//!
//! 通常の inbound Move は `routes::inbox` で HTTP 署名検証 → `dispatch::handle`
//! → `dispatch::move_handler::handle_move` の流れで自動受理される。
//! 受領時に DB / network の **一時障害** で 503 を返してしまったケース
//! (Mastodon は指数バックオフで再送するが、再送が止まったあとに残らず捨てる
//! こともある) を手動でリトライするための薄いラッパが本 CLI。
//!
//! # 流れ
//!
//! 1. `--from <file>` の JSON を読み込む。
//! 2. `actor` フィールド (= signer) を抽出し、`--signer` でオーバライド可能。
//! 3. signer actor を DB から `get_by_ap_id` で引く。存在しない場合は
//!    `remote_actor::fetch_and_upsert` で取り込む (HTTP 署名検証経路に
//!    乗らないがレスポンスは JSON only)。
//! 4. `dispatch::move_handler::handle_move` を直接呼ぶ。`alsoKnownAs`
//!    同意検査・target fetch・自動 re-follow まで本来の経路と同じ判定を
//!    通る (Malformed なら永続的拒否、Internal なら一時的失敗)。
//!
//! # 注意 — HTTP 署名検証は通らない
//!
//! 本 CLI は **インバウンドの HTTP 署名検証を通らない** ため、信頼できる
//! 入力 (= 自分が控えておいた activity 本文) でのみ実行すること。第三者
//! から渡された JSON を流すと「Move を勝手に偽装」の入り口になりかねない。

use anyhow::{Context, bail};
use sakurasato_core::model::ActorRow;
use sakurasato_core::{Config, repo};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::cli::MoveAcceptArgs;
use crate::dispatch::move_handler;
use crate::remote_actor::{self, FetchError};
use crate::state::AppState;

pub async fn run(config: Config, args: MoveAcceptArgs) -> anyhow::Result<()> {
    let bytes = std::fs::read(&args.from)
        .with_context(|| format!("read activity JSON from {}", args.from.display()))?;
    let activity: JsonValue = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse activity JSON from {}", args.from.display()))?;

    let signer_uri = if let Some(uri) = args.signer.as_deref() {
        info!(signer = uri, "move-accept: using --signer override");
        uri.to_string()
    } else {
        extract_actor_uri(&activity)
            .ok_or_else(|| anyhow::anyhow!("activity has no usable `actor` URI; use --signer"))?
            .to_string()
    };

    let activity_type = activity
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    if activity_type != "Move" {
        bail!(
            "activity `type` is {activity_type:?}; expected \"Move\". \
             Use a different CLI or fix the JSON file.",
        );
    }

    let state = AppState::from_config(config).await?;
    let signer = ensure_signer(&state, &signer_uri).await?;

    move_handler::handle_move(&state, &signer, &activity)
        .await
        .with_context(|| format!("re-process Move from {signer_uri}"))?;

    println!(
        "Move re-processed: signer={signer_uri} (signer_id={id})",
        id = signer.id,
    );
    Ok(())
}

async fn ensure_signer(state: &AppState, ap_id: &str) -> anyhow::Result<ActorRow> {
    if let Some(existing) = repo::actor::get_by_ap_id(state.pool(), ap_id)
        .await
        .context("lookup signer actor")?
    {
        return Ok(existing);
    }
    info!(signer = ap_id, "move-accept: signer not in DB; fetching");
    remote_actor::fetch_and_upsert(state, ap_id)
        .await
        .map_err(|e| match e {
            FetchError::Blocked { host, reason } => {
                anyhow::anyhow!("signer fetch blocked: host {host:?} → {reason}")
            }
            FetchError::Malformed(msg) => anyhow::anyhow!("signer actor malformed: {msg}"),
            FetchError::Db(err) => anyhow::Error::new(err).context("upsert signer actor"),
            other => anyhow::anyhow!("signer fetch failed: {other}"),
        })
}

/// `actor` フィールドから URI を取る。文字列 / `{"id": "..."}` 両方対応。
fn extract_actor_uri(activity: &JsonValue) -> Option<&str> {
    match activity.get("actor")? {
        JsonValue::String(s) => Some(s.as_str()),
        JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_actor_uri_accepts_string_and_object() {
        let a = json!({"actor": "https://x/users/a"});
        assert_eq!(extract_actor_uri(&a), Some("https://x/users/a"));

        let b = json!({"actor": {"id": "https://x/users/b", "type": "Person"}});
        assert_eq!(extract_actor_uri(&b), Some("https://x/users/b"));

        let c = json!({"actor": {"type": "Person"}});
        assert_eq!(extract_actor_uri(&c), None);

        let d = json!({});
        assert_eq!(extract_actor_uri(&d), None);
    }
}
