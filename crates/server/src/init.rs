use anyhow::{Context, anyhow, bail};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sakurasato_core::{Config, MIGRATOR, repo};
use tokio::task;
use tracing::{info, warn};

use crate::cli::InitArgs;
use crate::state::AppState;

const RSA_BITS: usize = 2048;

pub async fn run(config: Config, args: InitArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;

    MIGRATOR
        .run(state.pool())
        .await
        .context("apply pending DB migrations")?;

    let username = args
        .username
        .clone()
        .unwrap_or_else(|| state.config().server.user.clone());
    if username.is_empty() {
        bail!("username is empty; set server.user in config or pass --username");
    }
    let host = state.config().server.host.clone();
    let ap_id = state.local_actor_ap_id(&username);

    let existing = repo::actor::get_by_ap_id(state.pool(), &ap_id)
        .await
        .context("check for existing actor")?;
    if existing.is_some() && !args.force {
        info!(
            ap_id = %ap_id,
            "local actor already exists — pass --force to re-key (destructive)",
        );
        return Ok(());
    }
    if existing.is_some() {
        warn!(
            ap_id = %ap_id,
            "--force requested: re-keying the local actor will break federation with anyone who cached the old public key",
        );
    }

    info!(bits = RSA_BITS, "generating RSA signing key");
    // RSA 鍵生成は数秒の CPU バウンド処理なので、Tokio ランタイムスレッドを
    // ブロックしないように spawn_blocking でオフロードする。
    let (private_pem, public_pem) = task::spawn_blocking(|| -> anyhow::Result<_> {
        let mut rng = OsRng;
        let private_key = RsaPrivateKey::new(&mut rng, RSA_BITS)
            .map_err(|e| anyhow!("failed to generate RSA key: {e}"))?;
        let public_key = RsaPublicKey::from(&private_key);
        let private_pem = private_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| anyhow!("encode private key as PKCS#8 PEM: {e}"))?
            .to_string();
        let public_pem = public_key
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| anyhow!("encode public key as SPKI PEM: {e}"))?;
        Ok((private_pem, public_pem))
    })
    .await
    .context("RSA keygen task panicked")??;

    let new = repo::actor::NewActor {
        ap_id: ap_id.clone(),
        preferred_username: username.clone(),
        host,
        display_name: args.display_name.or_else(|| Some(username.clone())),
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{}/inbox", state.config().server.host)),
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: Some(format!("{ap_id}/followers")),
        following_url: Some(format!("{ap_id}/following")),
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: public_pem,
        private_key_pem: Some(private_pem),
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
    };

    // 既存削除と新規挿入は同一トランザクションで実行する。途中でクラッシュ
    // しても actor を消したまま終わる事故を防ぐ。
    let mut tx = state.pool().begin().await.context("begin transaction")?;
    if let Some(prev) = existing.as_ref() {
        repo::actor::delete_by_id(&mut *tx, prev.id)
            .await
            .context("delete previous local actor")?;
    }
    let inserted = repo::actor::insert(&mut *tx, new)
        .await
        .context("insert local actor")?;
    tx.commit().await.context("commit init transaction")?;
    info!(
        id = inserted.id,
        ap_id = %inserted.ap_id,
        "local actor created",
    );
    Ok(())
}
