//! Axum extractor: 署名検証済み inbox リクエスト。
//!
//! `SignedInboxBody` は body を `Bytes` に吸い、`Digest` / `Content-Digest`
//! を検証してから署名 base を組み立てて、`Signature` (cavage) または
//! `Signature-Input` + `Signature` (RFC 9421) でアルゴリズムを判定し、
//! actor を DB から引いて RSA / Ed25519 のどちらかで検証する。検証成功時
//! のみ `SignedInboxBody { actor, body, scheme, key_kind }` を後段の
//! handler に渡す。
//!
//! 失敗は [`crate::sign::SigError`] 経由で `IntoResponse` で 401 / 400 を
//! 返し、handler に到達させない。これにより未知 actor (DB に無い keyId) は
//! M3b-2 では一律 401 で弾かれ、remote actor fetch は M3b-3 に委ねられる。
//!
//! ボディサイズ上限は 1 MiB。ActivityPub Activity JSON は数 KB 以下が
//! 標準で、これを超えるものは攻撃か誤設定の可能性が高い。

use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::http::Request;
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;

use crate::remote_actor;
use crate::sign::{self, RequestContext, SigError, SigScheme, keyid};
use crate::state::AppState;

/// POST inbox 本体に渡される、署名検証済みのリクエスト情報。
#[derive(Debug)]
pub(crate) struct SignedInboxBody {
    /// 送信元 actor。RSA 鍵で署名なら local の `ActorRow` でも remote の
    /// 何でも、`keyId` に対応する actor が DB に存在する場合に限り渡る。
    pub(crate) actor: ActorRow,
    /// 生 body bytes。後段の handler が JSON として再パースする。
    pub(crate) body: Bytes,
    /// 検証に使った署名スキーム (cavage / RFC 9421)。
    pub(crate) scheme: SigScheme,
    /// 使用した鍵種別 (RSA / Ed25519)。
    pub(crate) key_kind: keyid::KeyKind,
}

/// inbox に到達するリクエストの最大ボディサイズ (1 MiB)。
const MAX_BODY_BYTES: usize = 1024 * 1024;

impl<B> FromRequest<AppState, B> for SignedInboxBody
where
    B: Send + 'static,
    Bytes: FromRequest<AppState, B>,
{
    type Rejection = SigError;

    async fn from_request(
        req: Request<axum::body::Body>,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 1. method / uri / headers をリクエストから退避してから body を吸う。
        let method = req.method().clone();
        let uri = req.uri().clone();
        let headers = req.headers().clone();
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|_| SigError::SignatureMalformed("body too large or unreadable".into()))?;

        // 2. 署名スキームと keyId を抽出 (DB アクセスなし)。
        let info = sign::extract_signature_info(&headers)?;

        // 3. keyId → ap_id を取り出して actor を引く。M3b-3 PR2 以降は
        // DB に無い場合 remote から fetch して upsert を試みる。
        // DB エラーと「actor が存在しない」を分離 (F6): DB 障害なら
        // 503 を返して Mastodon 系の長めのリトライ保持に乗せる。
        let parsed_keyid =
            keyid::parse(&info.key_id).map_err(|e| SigError::KeyIdMalformed(e.to_string()))?;
        let ap_id = parsed_keyid.ap_id;
        let actor = match repo::actor::get_by_ap_id(state.pool(), ap_id).await {
            Ok(Some(row)) => row,
            Ok(None) if !state.enable_remote_fetch() => {
                // テスト経路 (`AppState::from_pool`) では remote fetch を
                // 無効化する ── 統合テストが実 DNS / 実ネットワークに到達
                // しないようにするため。未知 keyId は即 401 で再送ループに
                // 乗せる (PR1 と同じ挙動)。
                return Err(SigError::UnknownActor);
            }
            Ok(None) => {
                // 未知 actor → remote fetch を試みる。SSRF ガード・redirect
                // 拒否・size 上限は [`remote_actor`] が責任を持つ。失敗は
                // 401 (UnknownActor) に倒して Mastodon の再送ループに乗せる。
                tracing::info!(ap_id, "actor not in DB; attempting remote fetch");
                match remote_actor::fetch_and_upsert_for_signature(state, ap_id).await {
                    Ok(row) => row,
                    Err(remote_actor::FetchError::Db(e)) => {
                        tracing::error!(error = %e, ap_id, "DB error during remote actor upsert");
                        return Err(SigError::Internal);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, ap_id, "remote actor fetch failed");
                        return Err(SigError::UnknownActor);
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, ap_id, "DB error during actor lookup");
                return Err(SigError::Internal);
            }
        };

        // 4. target_uri と path_and_query を組み立てる。
        let path_and_query = uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
        let target_uri = format!(
            "https://{host}{path_and_query}",
            host = state.config().server.host,
        );

        // 5. 検証本体。失敗時はそのまま SigError として 401 / 400 を返す。
        let ctx = RequestContext {
            method: method.as_str(),
            path_and_query: &path_and_query,
            target_uri: &target_uri,
            headers: &headers,
            body: &body,
        };
        sign::verify_request_with_actor(&ctx, &info, &actor)?;

        tracing::info!(
            scheme = ?info.scheme,
            key_kind = ?info.key_kind,
            actor_ap_id = %actor.ap_id,
            "inbox signature verified",
        );

        Ok(Self {
            actor,
            body,
            scheme: info.scheme,
            key_kind: info.key_kind,
        })
    }
}
