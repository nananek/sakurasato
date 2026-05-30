use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "sakurasato-server",
    about = "Sakurasato — お一人様 Fediverse server"
)]
pub struct Cli {
    /// Path to an additional config TOML overlay (loaded on top of
    /// `config/default.toml`).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the AP federation daemon (HTTP + local Unix socket).
    Serve,
    /// Bootstrap the single local user actor and instance.actor.
    Init(InitArgs),
    /// Attempt a single outbound delivery from the queue. Used in M3b-2 to
    /// hand-fire delivery rows from local federation tests (M3b-3 will
    /// replace this with a resident worker loop).
    Deliver(DeliverArgs),
    /// Manage local API tokens (Bearer auth for the Unix-socket API).
    Token(TokenArgs),
    /// Manage custom emojis (M8).
    Emoji(EmojiArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Override the username from `config.server.user` (advanced).
    #[arg(long)]
    pub username: Option<String>,
    /// Display name for the local actor (defaults to the username).
    #[arg(long)]
    pub display_name: Option<String>,
    /// Re-initialise even if a local actor already exists (re-issues the
    /// signing key — break federation, destructive).
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct DeliverArgs {
    /// `delivery_queue.id` to flush. Use `psql` (or the local API once it
    /// exists in M4) to enumerate ids; this CLI deliberately does not list
    /// the queue to keep the surface tiny.
    #[arg(long)]
    pub queue_id: i64,
}

#[derive(Debug, Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub command: TokenCommand,
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Issue a new token and print the raw value once to stdout. This is
    /// the only time the raw token is shown — the DB only stores its hash.
    Issue(TokenIssueArgs),
    /// List existing tokens (id / name / created / `last_used`). The raw
    /// token is intentionally not re-printed; reissue via `revoke` + `issue`.
    List,
    /// Hard-delete a token by id (from `list`).
    Revoke(TokenRevokeArgs),
}

#[derive(Debug, Args)]
pub struct TokenIssueArgs {
    /// Human-readable label (e.g. "tui-laptop"). Duplicates are allowed.
    #[arg(long)]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct TokenRevokeArgs {
    /// `api_token.id` from `sakurasato token list`.
    #[arg(long)]
    pub id: i64,
}

#[derive(Debug, Args)]
pub struct EmojiArgs {
    #[command(subcommand)]
    pub command: EmojiCommand,
}

#[derive(Debug, Subcommand)]
pub enum EmojiCommand {
    /// Import a Misskey-format emoji zip (`meta.json` + image files).
    ///
    /// 既存 shortcode は **上書き** される (CLAUDE.md §5.4)。本コマンドは
    /// 画像バイト列を server 本体ではデコードせず、`media-proxy` の
    /// `/v1/image/sanitize` 経由で再エンコードしてから versitygw に書く ──
    /// CLAUDE.md §7 の隔離方針を維持するため。
    Import(EmojiImportArgs),
}

#[derive(Debug, Args)]
pub struct EmojiImportArgs {
    /// Path to a Misskey-format emoji zip.
    pub zip: PathBuf,
}
