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
