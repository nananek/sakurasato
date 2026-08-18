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
//! **連合ドメインブロック (PR5)**: actor 解決直後に
//! `repo::domain_moderation::get_by_host` を見て、そのドメインが suspend
//! 対象なら crypto 検証を試みずに 403 (`SigError::DomainSuspended`) で
//! 拒否する。silence は本 extractor では判定しない (inbox 受信自体は許可
//! し、効果を新規 Follow 拒否のみに限定するため。計画書 §10 確定事項 #3)。
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

        // 2. 署名スキームと keyId を抽出 (DB アクセスなし)。RFC 9421 で
        // 複数ラベルが届いた場合はすべてのラベルが返る。
        let infos = sign::extract_signature_infos(&headers)?;

        // 3. target_uri と path_and_query を組み立てる (ラベルごとに同じ)。
        let path_and_query = uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
        let target_uri = format!(
            "https://{host}{path_and_query}",
            host = state.config().server.host,
        );
        let ctx = RequestContext {
            method: method.as_str(),
            path_and_query: &path_and_query,
            target_uri: &target_uri,
            headers: &headers,
            body: &body,
        };

        // 4. 各ラベルを順に試行し、いずれか一つが完全検証できた時点で
        // 受理する (multi-label = OR セマンティクス)。
        //
        // 401 で再送ループに乗せて良いエラー (UnknownActor / BadSignature
        // など) はラベルを跨いでスキップ。一方 503 級 (`SigError::Internal`)
        // は DB 障害 → 即時 503 で送信側のリトライ保持を長く取らせる。
        let mut last_err: Option<SigError> = None;
        for info in infos {
            let parsed_keyid = match keyid::parse(&info.key_id) {
                Ok(p) => p,
                Err(e) => {
                    last_err = Some(SigError::KeyIdMalformed(e.to_string()));
                    continue;
                }
            };
            let ap_id = parsed_keyid.ap_id;
            let actor = match repo::actor::get_by_ap_id(state.pool(), ap_id).await {
                Ok(Some(row)) => row,
                Ok(None) if !state.enable_remote_fetch() => {
                    // テスト経路 (`AppState::from_pool`) では remote fetch を
                    // 無効化する。未知 keyId は次のラベルがあればそちらを試行、
                    // 無ければ 401 で再送ループに乗せる (PR1 と同じ挙動)。
                    last_err = Some(SigError::UnknownActor);
                    continue;
                }
                Ok(None) => {
                    // 未知 actor → remote fetch を試みる。SSRF ガード・
                    // redirect 拒否・size 上限は [`remote_actor`] が責任を持つ。
                    tracing::info!(ap_id, "actor not in DB; attempting remote fetch");
                    match remote_actor::fetch_and_upsert_for_signature(state, ap_id).await {
                        Ok(row) => row,
                        Err(remote_actor::FetchError::Db(e)) => {
                            tracing::error!(error = %e, ap_id, "DB error during remote actor upsert");
                            // DB 障害は他ラベルでも失敗確実 → 即 503 を返す。
                            return Err(SigError::Internal);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, ap_id, "remote actor fetch failed");
                            last_err = Some(SigError::UnknownActor);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, ap_id, "DB error during actor lookup");
                    // DB 障害は他ラベルでも失敗確実 → 即 503 を返す。
                    return Err(SigError::Internal);
                }
            };

            // 4.5. 連合ドメインブロック (PR5、計画書 §6.4): suspend 対象
            // ドメインの actor は crypto 検証を試みる前に拒否する (検証
            // コスト削減)。silence は inbox 受信自体を妨げない (§10 確定
            // 事項 #3、効果は handle_follow 側のガードに限定)。
            match repo::domain_moderation::get_by_host(state.pool(), &actor.host).await {
                Ok(Some(m)) if m.severity == "suspend" => {
                    tracing::warn!(
                        actor_ap_id = %actor.ap_id,
                        host = %actor.host,
                        "inbox rejected: actor's domain is suspended",
                    );
                    return Err(SigError::DomainSuspended);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, host = %actor.host, "DB error during domain moderation lookup");
                    return Err(SigError::Internal);
                }
            }

            // 5. 検証本体。成功すればここで return。失敗は次ラベルへ。
            let scheme = info.scheme;
            let key_kind = info.key_kind;
            match sign::verify_request_with_actor(&ctx, &info, &actor) {
                Ok(()) => {
                    tracing::info!(
                        scheme = ?scheme,
                        key_kind = ?key_kind,
                        label = ?info.label,
                        actor_ap_id = %actor.ap_id,
                        "inbox signature verified",
                    );
                    return Ok(Self {
                        actor,
                        body,
                        scheme,
                        key_kind,
                    });
                }
                Err(SigError::Internal) => {
                    // 内部障害はラベルを跨いでも改善しないので即返す。
                    return Err(SigError::Internal);
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        // どのラベルも通らなかった。最後のエラーで応答する。`infos` は空に
        // ならない (`extract_signature_infos` が 0 件で `Empty` を返す) ので
        // 通常 `last_err` は必ず Some。
        Err(last_err.unwrap_or(SigError::BadSignature))
    }
}
