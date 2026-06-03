//! `MiAuth` 管理 CLI (M14 #157, 親 issue #150)。
//!
//! Misskey 互換クライアントからの認可フローを **CLI で承認** することで、
//! CLAUDE.md §1 の「Web 認証 UI は作らない」原則を維持しつつ `MiAuth` wire
//! 互換性を提供する。
//!
//! ## サブコマンド
//!
//! - `miauth list` ── pending session を一覧 (`requested_at` 古い順)。
//! - `miauth approve <UUID> --permission a,b,c` ── 承認 + token 発行。
//!   生 token はここでだけ stdout に出る (以降は DB に hash しか残らない)。
//! - `miauth reject <UUID>` ── 拒否。`POST /api/miauth/{uuid}/check` は
//!   以降 404 を返す。
//! - `miauth tokens` ── 発行済み token を一覧 (revoke 対象探索用)。
//! - `miauth revoke --id N` ── token をハード削除。session は監査トレイル
//!   として残るが `issued_token_id` は NULL に倒れる。
//!
//! ## permission scope の指定
//!
//! `--permission` は CSV (= `read:account,write:reactions`)。空白は trim、
//! 重複は dedup、空文字は拒否。Misskey は 70+ scope を持つが、ここでは値の
//! validate を行わず文字列のまま保管する ── 未知 scope を弾く CHECK を入れ
//! ると Misskey 側の新規 scope 追加で本コマンドが壊れるため、scope の
//! enforcement は **endpoint 側** (= #158 以降の handler が `has_scope` で
//! hardcoded scope と照合する) で行う。

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use sakurasato_core::{Config, repo};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Args)]
pub struct MiAuthArgs {
    #[command(subcommand)]
    pub command: MiAuthCommand,
}

#[derive(Debug, Subcommand)]
pub enum MiAuthCommand {
    /// pending session の一覧 (`requested_at` 古い順)。期限切れは表示前に
    /// `expired` 状態に倒される (= 自動 sweep)。
    List,
    /// pending session を **承認** し、Bearer-相当の token を 1 つ発行する。
    /// `--permission` で snapshot する scope を指定する (= ユーザが client
    /// 要求より絞ることもできる)。
    Approve(MiAuthApproveArgs),
    /// pending session を **拒否**。以降の `POST /api/miauth/{uuid}/check`
    /// は 404 を返す。
    Reject(MiAuthRejectArgs),
    /// 発行済み `MiAuth` token を一覧する (revoke 対象の探索用)。`api_token`
    /// (= TUI 用) とは別系統で、混在しない。
    Tokens,
    /// `miauth_token.id` をハード削除する。client 側は次の API 呼び出しで
    /// 401 を受けて再認可フローに入る (= Misskey クライアントは `MiAuth` UUID
    /// を再生成して `GET /miauth/{uuid}` を再度開く)。
    Revoke(MiAuthRevokeArgs),
}

#[derive(Debug, Args)]
pub struct MiAuthApproveArgs {
    /// 承認対象の session UUID (= browser landing `/miauth/{uuid}` 経路で
    /// 発行されたもの)。`miauth list` で一覧から拾う。
    pub uuid: String,
    /// 付与する permission scope の CSV (= `read:account,write:reactions`)。
    /// 1 個以上必須。Misskey の scope は string set で、Sakurasato 側では
    /// validate しない (= 未知 scope を弾くと Misskey 側の追加で壊れるため、
    /// endpoint 側で hardcoded list と突き合わせる)。
    #[arg(long, value_delimiter = ',', required = true)]
    pub permission: Vec<String>,
}

#[derive(Debug, Args)]
pub struct MiAuthRejectArgs {
    /// 拒否対象の session UUID。
    pub uuid: String,
}

#[derive(Debug, Args)]
pub struct MiAuthRevokeArgs {
    /// `miauth_token.id`。`miauth tokens` で一覧から拾う。
    #[arg(long)]
    pub id: i64,
}

/// `sakurasato-server miauth ...` のエントリポイント。
pub async fn run(config: Config, args: MiAuthArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        MiAuthCommand::List => run_list(&state).await,
        MiAuthCommand::Approve(approve) => run_approve(&state, approve).await,
        MiAuthCommand::Reject(reject) => run_reject(&state, reject).await,
        MiAuthCommand::Tokens => run_tokens(&state).await,
        MiAuthCommand::Revoke(revoke) => run_revoke(&state, revoke).await,
    }
}

async fn run_list(state: &AppState) -> anyhow::Result<()> {
    let sessions = repo::miauth::list_pending_sessions(state.pool())
        .await
        .context("list pending miauth sessions")?;
    if sessions.is_empty() {
        println!("(no pending sessions)");
        return Ok(());
    }
    for row in sessions {
        println!(
            "uuid={} app={:?} requested={} expires={} permission_request={:?}",
            row.uuid,
            row.app_name,
            row.requested_at.to_rfc3339(),
            row.expires_at.to_rfc3339(),
            row.permissions.0,
        );
    }
    Ok(())
}

async fn run_approve(state: &AppState, args: MiAuthApproveArgs) -> anyhow::Result<()> {
    let uuid = parse_uuid(&args.uuid)?;
    let permissions = normalize_permissions(&args.permission)?;
    // M14 #158: CLI は **CAS pending → approved のみ** を行う。token 発行は
    // `POST /api/miauth/{uuid}/check` 側で Misskey クライアントが polling
    // で受け取る経路に統一する (= 既存 Misskey クライアント wire 仕様)。
    // 旧 #157 実装では `approve` が token 発行 + stdout 表示 + consumed まで
    // 一気に進めていたが、これでは Milktea / `MissRirica` が token を受け取れ
    // ない (= polling レスポンスに raw が乗らない) ため、wire 互換性のため
    // 役割分担を切り替えた。
    let cas = repo::miauth::approve_session(state.pool(), uuid, &permissions)
        .await
        .with_context(|| format!("CAS approve miauth_session {uuid}"))?;
    if cas == 0 {
        // pending では無かった ── 現状を再確認してユーザに状況を伝える。
        let row = repo::miauth::get_session(state.pool(), uuid)
            .await
            .with_context(|| format!("look up miauth_session {uuid} after CAS"))?;
        match row {
            None => bail!("no session with uuid={uuid}"),
            Some(r) => bail!(
                "session uuid={uuid} is not pending (current state={:?}); \
                 nothing to approve",
                r.state
            ),
        }
    }
    eprintln!("approved session uuid={uuid} permissions={permissions:?}");
    eprintln!("(the client will now receive its token via POST /api/miauth/{uuid}/check polling)");
    Ok(())
}

async fn run_reject(state: &AppState, args: MiAuthRejectArgs) -> anyhow::Result<()> {
    let uuid = parse_uuid(&args.uuid)?;
    let cas = repo::miauth::reject_session(state.pool(), uuid)
        .await
        .with_context(|| format!("CAS reject miauth_session {uuid}"))?;
    if cas == 0 {
        let row = repo::miauth::get_session(state.pool(), uuid)
            .await
            .with_context(|| format!("look up miauth_session {uuid} after CAS"))?;
        match row {
            None => bail!("no session with uuid={uuid}"),
            Some(r) => bail!(
                "session uuid={uuid} is not pending (current state={:?}); \
                 nothing to reject",
                r.state
            ),
        }
    }
    eprintln!("rejected session uuid={uuid}");
    Ok(())
}

async fn run_tokens(state: &AppState) -> anyhow::Result<()> {
    let tokens = repo::miauth::list_tokens(state.pool())
        .await
        .context("list miauth_token rows")?;
    if tokens.is_empty() {
        println!("(no tokens issued)");
        return Ok(());
    }
    for row in tokens {
        let last = row
            .last_used_at
            .map_or_else(|| "never".to_string(), |t| t.to_rfc3339());
        println!(
            "id={} name={:?} permissions={:?} created={} last_used={}",
            row.id,
            row.name,
            row.permissions.0,
            row.created_at.to_rfc3339(),
            last,
        );
    }
    Ok(())
}

async fn run_revoke(state: &AppState, args: MiAuthRevokeArgs) -> anyhow::Result<()> {
    let deleted = repo::miauth::delete_token_by_id(state.pool(), args.id)
        .await
        .with_context(|| format!("revoke miauth_token id={}", args.id))?;
    if deleted {
        println!("revoked miauth_token id={}", args.id);
        Ok(())
    } else {
        bail!("no miauth_token with id={}", args.id);
    }
}

fn parse_uuid(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw.trim()).with_context(|| format!("invalid UUID: {raw:?}"))
}

/// `--permission` を正規化する:
/// - 各要素を `trim`
/// - 空文字は削除
/// - 重複は除去 (= 順序は保持、初出を残す)
/// - 結果が空なら error
fn normalize_permissions(raw: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for s in raw {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    if out.is_empty() {
        bail!("--permission must contain at least one non-empty scope");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_permissions_dedups_and_trims() {
        let input = vec![
            "read:account".to_string(),
            " write:reactions ".to_string(),
            "read:account".to_string(),
            String::new(),
            "  ".to_string(),
        ];
        let normalized = normalize_permissions(&input).unwrap();
        assert_eq!(
            normalized,
            vec!["read:account".to_string(), "write:reactions".to_string()]
        );
    }

    #[test]
    fn normalize_permissions_rejects_empty() {
        let input = vec![String::new(), "   ".to_string()];
        assert!(normalize_permissions(&input).is_err());
    }

    #[test]
    fn parse_uuid_strict() {
        let u = parse_uuid("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(u.to_string(), "550e8400-e29b-41d4-a716-446655440000");
        // 前後 whitespace は許容。
        let u2 = parse_uuid("  550e8400-e29b-41d4-a716-446655440000  ").unwrap();
        assert_eq!(u2, u);
        // 不正値は拒否。
        assert!(parse_uuid("not-a-uuid").is_err());
        assert!(parse_uuid("").is_err());
    }
}
