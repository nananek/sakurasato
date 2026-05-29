//! Inbox endpoints.
//!
//! M3a で受け口 (`POST /inbox`, `POST /users/<name>/inbox`) を 202 で返す
//! だけのプレースホルダとして開けた。M3b-2 で **HTTP 署名検証**を
//! [`crate::extract::SignedInboxBody`] extractor に集約し、検証が通った
//! リクエストのみがこの handler に届く。
//!
//! 現段階 (M3b-2) では検証通過後に `tracing::info!` でログを残して 202 を
//! 返すだけ。Activity ディスパッチ (Follow / Accept / Reject / Create /
//! Delete 等の本処理) は **M3b-3 以降**で順次実装する。
//!
//! 未知 actor (`keyId` が DB に無い) は extractor が 401 を返すため、
//! Mastodon 系の再送ループに乗る。M3b-3 で remote actor fetch を実装した
//! あとに自然に検証成立する設計。

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::extract::SignedInboxBody;

pub(crate) async fn shared_inbox(signed: SignedInboxBody) -> Response {
    log_and_accept(&signed, None)
}

pub(crate) async fn user_inbox(Path(name): Path<String>, signed: SignedInboxBody) -> Response {
    log_and_accept(&signed, Some(name.as_str()))
}

fn log_and_accept(signed: &SignedInboxBody, recipient: Option<&str>) -> Response {
    // body の中身は M3b-3 以降の Activity ディスパッチ実装で JSON パース
    // するため、ここでは ap_id と scheme のみログに残す。秘密情報は出さない。
    //
    // ## M3b-3 dispatch 実装時に **必ず** 行うべき検証 (F3 由来)
    //
    // 1. **Activity の `actor` フィールドと `signed.actor.ap_id` の一致**:
    //    body の JSON をパースして得た `actor` URI が `signed.actor.ap_id`
    //    と一致しない場合は 401 で拒否する。一致を取らないと、`evil.example`
    //    の有効アカウントが任意のリモート actor (`good.example/users/...`)
    //    を装った Activity を inbox に送り込める。
    // 2. **Activity 内で参照される object の `attributedTo` も同 actor**:
    //    `Create{Note{attributedTo: ...}}` などネストした actor 参照も
    //    署名者と一致することを確認。
    // 3. **`Move` / `Delete` 等の高権限アクティビティは追加検証**:
    //    `Move` は双方向合意 (movedTo ↔ alsoKnownAs)、`Delete` は対象
    //    object の actor 一致を厳密に。
    //
    // これらは extractor (本 PR) のスコープではなく、dispatch 実装側の
    // 責務。レビュー (PR #19 Finding 3) で明示的に指摘済み。
    tracing::info!(
        actor = %signed.actor.ap_id,
        scheme = ?signed.scheme,
        key_kind = ?signed.key_kind,
        recipient = ?recipient,
        body_size = signed.body.len(),
        "inbox accepted (activity dispatch pending in M3b-3)",
    );
    (
        StatusCode::ACCEPTED,
        "accepted; activity dispatch pending (M3b-3)",
    )
        .into_response()
}
