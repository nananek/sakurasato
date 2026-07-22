//! `sakurasato-server list ...` ── リスト機能 (Mastodon/Misskey 互換) の
//! 管理 CLI。MiAuth (`crate::miauth::lists`, Aria 等向け) / TUI ローカル API
//! (`crate::local_api::user_list`) と同じ [`repo::user_list`] を操作する
//! フォールバック経路 (= Aria が無くてもサーバ側だけで管理できる)。

use anyhow::{Context, bail};
use sakurasato_core::Config;
use sakurasato_core::repo;
use sakurasato_core::repo::user_list::AddMemberError;

use crate::cli::{
    ListArgs, ListCommand, ListCreateArgs, ListIdArgs, ListMemberArgs, ListRenameArgs,
};
use crate::state::AppState;

pub async fn run(config: Config, args: ListArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        ListCommand::Create(a) => create(&state, a).await,
        ListCommand::List => list(&state).await,
        ListCommand::Show(a) => show(&state, a).await,
        ListCommand::Rename(a) => rename(&state, a).await,
        ListCommand::Delete(a) => delete(&state, a).await,
        ListCommand::AddMember(a) => add_member(&state, a).await,
        ListCommand::RemoveMember(a) => remove_member(&state, a).await,
    }
}

async fn create(state: &AppState, args: ListCreateArgs) -> anyhow::Result<()> {
    let title = args.title.trim();
    if title.is_empty() {
        bail!("--title must not be empty");
    }
    let row = repo::user_list::create(state.pool(), title)
        .await
        .context("insert user_list row")?;
    println!("created list id={} title={:?}", row.id, row.title);
    Ok(())
}

async fn list(state: &AppState) -> anyhow::Result<()> {
    let rows = repo::user_list::list_all_with_counts(state.pool())
        .await
        .context("list user_list rows")?;
    if rows.is_empty() {
        println!("(no lists)");
        return Ok(());
    }
    println!("ID\tMEMBERS\tTITLE");
    for row in rows {
        println!("{}\t{}\t{}", row.list.id, row.member_count, row.list.title);
    }
    Ok(())
}

async fn show(state: &AppState, args: ListIdArgs) -> anyhow::Result<()> {
    let Some(row) = repo::user_list::get_by_id(state.pool(), args.id)
        .await
        .context("lookup user_list row")?
    else {
        bail!("no list with id={}", args.id);
    };
    let member_ids = repo::user_list::list_member_ids(state.pool(), args.id)
        .await
        .context("list user_list_member ids")?;
    let members = repo::actor::list_by_ids(state.pool(), &member_ids)
        .await
        .context("resolve member actors")?;
    println!("id={} title={:?}", row.id, row.title);
    if members.is_empty() {
        println!("(no members)");
        return Ok(());
    }
    println!("MEMBER_ID\tACCT");
    for m in members {
        println!("{}\t{}@{}", m.id, m.preferred_username, m.host);
    }
    Ok(())
}

async fn rename(state: &AppState, args: ListRenameArgs) -> anyhow::Result<()> {
    let title = args.title.trim();
    if title.is_empty() {
        bail!("--title must not be empty");
    }
    let Some(row) = repo::user_list::rename(state.pool(), args.id, title)
        .await
        .context("rename user_list row")?
    else {
        bail!("no list with id={}", args.id);
    };
    println!("renamed list id={} title={:?}", row.id, row.title);
    Ok(())
}

async fn delete(state: &AppState, args: ListIdArgs) -> anyhow::Result<()> {
    let deleted = repo::user_list::delete_by_id(state.pool(), args.id)
        .await
        .context("delete user_list row")?;
    if deleted == 0 {
        bail!("no list with id={}", args.id);
    }
    println!("deleted list id={}", args.id);
    Ok(())
}

async fn add_member(state: &AppState, args: ListMemberArgs) -> anyhow::Result<()> {
    let local = resolve_local_actor_id(state).await?;
    let target = resolve_actor_spec(state, &args.actor).await?;
    match repo::user_list::add_member(state.pool(), args.id, local, target)
        .await
        .context("add list member")?
    {
        Ok(()) => {
            println!("added actor_id={target} to list id={}", args.id);
            Ok(())
        }
        Err(AddMemberError::ListNotFound) => bail!("no list with id={}", args.id),
        Err(AddMemberError::NotFollowing) => bail!(
            "actor_id={target} is not followed (state=accepted) by the local actor; follow first"
        ),
    }
}

async fn remove_member(state: &AppState, args: ListMemberArgs) -> anyhow::Result<()> {
    let target = resolve_actor_spec(state, &args.actor).await?;
    let removed = repo::user_list::remove_member(state.pool(), args.id, target)
        .await
        .context("remove list member")?;
    if removed == 0 {
        bail!("actor_id={target} is not a member of list id={}", args.id);
    }
    println!("removed actor_id={target} from list id={}", args.id);
    Ok(())
}

async fn resolve_local_actor_id(state: &AppState) -> anyhow::Result<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .context("lookup local actor")?;
    match row {
        Some(a) if a.is_local => Ok(a.id),
        _ => bail!("local actor not initialized; run `sakurasato-server init` first"),
    }
}

/// `--actor` は `acct` (`user@host`, `@` prefix 任意) または `actor.id`
/// (数値) のいずれか。リストに追加できるのは既にフォロー済み (=
/// ローカル DB に actor 行が存在する) 相手のみなので、WebFinger は経由
/// しない ── 常にローカル DB の行だけで解決が完結する。
async fn resolve_actor_spec(state: &AppState, spec: &str) -> anyhow::Result<i64> {
    if let Ok(id) = spec.parse::<i64>() {
        return Ok(id);
    }
    let acct = spec.strip_prefix('@').unwrap_or(spec);
    let Some((user, host)) = acct.split_once('@') else {
        bail!("--actor must be `user@host` or a numeric actor id, got {spec:?}");
    };
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .context("lookup actor by acct")?;
    row.map(|a| a.id)
        .ok_or_else(|| anyhow::anyhow!("no known actor for acct {spec:?}"))
}
