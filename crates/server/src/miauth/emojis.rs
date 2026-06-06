//! `POST /api/emojis` (= M14 #159, 親 issue #150)。
//!
//! Misskey 互換 ── ローカルカスタム絵文字を `{ emojis: MissEmoji[] }` で返す。
//!
//! ## scope
//!
//! Misskey 仕様では `emojis` は **anonymous** で叩ける (= public)。Sakurasato も
//! 同じ流儀で **scope 検査をスキップ** する。ただし `MiAuth` listener は
//! お一人様 server の内部経路で、外部公開時は cloudflared/Tailscale 越し
//! (= 認証境界はソケット側) で守られる前提なので、body `i` を持っていない
//! request も 200 で返す ── 互換性最優先。
//!
//! Sakurasato 既存 `/api/v1/emojis` (= Bearer 必須、TUI 用) と異なる挙動は意図的:
//! Misskey クライアント (Milktea 等) は emoji 一覧を login 前にも取りに来る
//! ことがある (= サーバ情報表示の一部)。
//!
//! ## clean-room
//!
//! - <https://api-doc.misskey.io/api/endpoints/emojis>
//!
//! observed wire shape (`emojis` を `/api/emojis` で本物 Misskey から取得):
//!
//! ```json
//! {
//!   "emojis": [
//!     {"aliases": [...], "name": "shortcode", "category": "...", "url": "https://..."}
//!   ]
//! }
//! ```

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};

use crate::local_api::media::build_media_url;
use crate::miauth::conv::MissEmoji;
use crate::miauth::error::internal_error;
use crate::state::AppState;

/// `MiAuth` listener が `/api/emojis` で返す件数上限。公開 discovery endpoint
/// ([`crate::routes::emojis`]) と共有するため core の値を参照する。
const EMOJIS_FETCH_LIMIT: i64 = sakurasato_core::repo::emoji::LIST_FETCH_LIMIT;

#[derive(Debug, Deserialize, Default)]
pub struct EmojisBody {
    /// auth 不要 (scope check はしない) だが、body shape として `i` は受け取る。
    /// Misskey クライアントは `i` を必ず付ける慣行 ── 互換のため `Option` で残す。
    #[serde(default)]
    pub i: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmojisResponse {
    pub emojis: Vec<MissEmoji>,
}

/// `POST /api/emojis` handler。
pub async fn handle(State(state): State<AppState>, body: Option<Json<EmojisBody>>) -> Response {
    // body は無視 ── `i` は受けるが scope 検証は走らせない (= Misskey の
    // anonymous-public 慣行に合わせる)。
    let _ = body;

    let rows = match repo::emoji::list_local_by_prefix(state.pool(), "", EMOJIS_FETCH_LIMIT).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "miauth /api/emojis: list_local_by_prefix failed");
            return internal_error("failed to list custom emojis");
        }
    };

    let host = &state.config().server.host;
    // Issue #135: `image_key` が None の row は image なし扱いで落とす
    // (= MiAuth クライアントは URL 必須の前提で picker を組むため)。
    let emojis: Vec<MissEmoji> = rows
        .into_iter()
        .filter_map(|row| {
            let image_key = row.image_key.as_deref()?;
            Some(MissEmoji {
                aliases: row.aliases.0,
                name: row.shortcode,
                category: row.category,
                url: build_media_url(host, image_key),
            })
        })
        .collect();
    Json(EmojisResponse { emojis }).into_response()
}
