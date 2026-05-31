use anyhow::{Context, anyhow, bail};
// pkcs8::EncodePrivateKey / EncodePublicKey は単一のトレイトで、ed25519-dalek
// と rsa の双方で同じ pkcs8 クレートが再エクスポートしているだけ。一度
// import すれば RSA / Ed25519 両方の `to_pkcs8_pem` / `to_public_key_pem` が
// メソッド解決で見える。
use ed25519_dalek::SigningKey as Ed25519SigningKey;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey, spki::der::pem::LineEnding};
use rsa::rand_core::OsRng;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sakurasato_core::{Config, MIGRATOR, repo};
use tokio::task;
use tracing::{info, warn};

use crate::cli::InitArgs;
use crate::state::AppState;

const RSA_BITS: usize = 2048;

/// Generated key material for a freshly-initialised local actor.
///
/// `Sakurasato` issues **両方** の鍵を同時に発行する。RSA は Mastodon を含む
/// cavage HTTP signatures 系の主流互換のため必須、Ed25519 は FEP-521a
/// `assertionMethod` 経由で公開し、RFC 9421 HTTP Message Signatures や
/// Iceshrimp / Sharkey 等の Ed25519 対応実装との連合に使う。M3b-1 では発行
/// と公開までを行い、署名生成・検証は M3b-2 で実装する。
// clippy::struct_field_names は "_pem" の suffix を嫌うが、ここでは PEM 形式で
// あることが本質情報なので明示的に許可する。
#[allow(clippy::struct_field_names)]
struct GeneratedKeys {
    rsa_private_pem: String,
    rsa_public_pem: String,
    ed25519_private_pem: String,
    ed25519_public_pem: String,
}

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

    info!(
        rsa_bits = RSA_BITS,
        "generating signing keys (RSA + Ed25519)"
    );
    // RSA 鍵生成は数秒の CPU バウンド処理、Ed25519 は瞬時だが OsRng の syscall
    // を伴うので、まとめて Tokio ランタイムスレッドの外 (spawn_blocking) で
    // 動かす。
    let keys = task::spawn_blocking(generate_keys)
        .await
        .context("keygen task panicked")??;

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
        public_key_pem: keys.rsa_public_pem,
        private_key_pem: Some(keys.rsa_private_pem),
        ed25519_public_key_id: Some(format!("{ap_id}#ed25519-key")),
        ed25519_public_key_pem: Some(keys.ed25519_public_pem),
        ed25519_private_key_pem: Some(keys.ed25519_private_pem),
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
        // 鍵アカフラグ (Issue #66 / M12):
        //   - 新規 init: `args.locked` をそのまま反映 (default = false)。
        //   - `--force` 再鍵化: 既存 lock 状態を **保つ**。`--locked` 単独で
        //     unlock → lock の片方向のみ可。lock → unlock したいときは
        //     再鍵化後に `actor unlock` を叩く運用 (= 鍵更新と state 変更を
        //     別操作に分離して、誤って lock を解除する事故を防ぐ)。
        manually_approves_followers: args.locked
            || existing
                .as_ref()
                .is_some_and(|e| e.manually_approves_followers),
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

fn generate_keys() -> anyhow::Result<GeneratedKeys> {
    let mut rng = OsRng;

    let rsa_private = RsaPrivateKey::new(&mut rng, RSA_BITS)
        .map_err(|e| anyhow!("failed to generate RSA key: {e}"))?;
    let rsa_public = RsaPublicKey::from(&rsa_private);
    let rsa_private_pem = rsa_private
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode RSA private key as PKCS#8 PEM: {e}"))?
        .to_string();
    let rsa_public_pem = rsa_public
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode RSA public key as SPKI PEM: {e}"))?;

    // ed25519-dalek の generate は CryptoRngCore を要求するので OsRng を共用。
    let ed_signing = Ed25519SigningKey::generate(&mut rng);
    let ed_verifying = ed_signing.verifying_key();
    let ed25519_private_pem = ed_signing
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode Ed25519 private key as PKCS#8 PEM: {e}"))?
        .to_string();
    let ed25519_public_pem = ed_verifying
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| anyhow!("encode Ed25519 public key as SPKI PEM: {e}"))?;

    Ok(GeneratedKeys {
        rsa_private_pem,
        rsa_public_pem,
        ed25519_private_pem,
        ed25519_public_pem,
    })
}
