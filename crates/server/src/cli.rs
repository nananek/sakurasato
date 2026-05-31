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
    /// Manage `alsoKnownAs` (= migration source candidates, M9).
    Alias(AliasArgs),
    /// Send a `Move` activity to followers (M9 引っ越し).
    ///
    /// 移動先 actor の `alsoKnownAs` に我々の `ap_id` が **先に** 載って
    /// いないと拒否する (相互同意検査)。実行すると local actor の
    /// `moved_to_ap_id` が target に倒れ、actor JSON が `movedTo` を返す
    /// ようになる。
    MoveOut(MoveOutArgs),
    /// Manually send a Follow activity to `<acct>` (M10).
    ///
    /// `acct:user@host` を `WebFinger` (media-proxy 経由) で解決し、
    /// `delivery_queue` に Follow を 1 行積む。常駐ワーカが拾って送る。
    /// 同じ相手にもう一度叩いても (`follower`, `followed`) UNIQUE と
    /// 決定論的な activity id で冪等。
    Follow(FollowArgs),
    /// Re-process an inbound `Move` activity from a saved JSON payload (M10).
    ///
    /// 初回受領時に DB / network エラーで 503 を返したケースを CLI で手動
    /// リトライするための薄いラッパ。
    /// `--from <file>` で activity 本文 (signer 含む) を読み、検証なしで
    /// `dispatch::move_handler::handle_move` を直接呼ぶ。
    MoveAccept(MoveAcceptArgs),
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

#[derive(Debug, Args)]
pub struct AliasArgs {
    #[command(subcommand)]
    pub command: AliasCommand,
}

#[derive(Debug, Subcommand)]
pub enum AliasCommand {
    /// List the current `alsoKnownAs` URIs.
    List,
    /// Add a URI to `alsoKnownAs` (idempotent) and broadcast an `Update`.
    Add(AliasMutateArgs),
    /// Remove a URI from `alsoKnownAs` (no-op if absent) and broadcast an `Update`.
    Remove(AliasMutateArgs),
    /// Clear all `alsoKnownAs` entries and broadcast an `Update`.
    Clear,
}

#[derive(Debug, Args)]
pub struct AliasMutateArgs {
    /// `ActivityPub` actor URI to add or remove (e.g. `https://old.example/users/me`).
    pub uri: String,
}

#[derive(Debug, Args)]
pub struct MoveOutArgs {
    /// 移動先 actor の `ActivityPub` URI。例: `https://new.example/users/me`。
    pub target: String,
    /// 双方向同意検査 (target の `alsoKnownAs` に自分が居るか) をスキップする。
    /// **緊急時のみ**: 同意が無いまま Move を投げると相手側で偽装と扱われる
    /// 可能性が高い (Mastodon は alsoKnownAs を必須にしている)。
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct FollowArgs {
    /// `acct:user@host` / `@user@host` / `user@host` のいずれでも可。
    /// `--actor-uri` を併用する場合は `WebFinger` 解決をスキップする。
    #[arg(default_value = "")]
    pub acct: String,
    /// `WebFinger` を経由せず直接 `ActivityPub` actor URI を指定する (オプション)。
    /// 例: `--actor-uri https://example.com/users/foo`。
    /// `acct` 引数があっても **こちらを優先** する。
    #[arg(long)]
    pub actor_uri: Option<String>,
}

#[derive(Debug, Args)]
pub struct MoveAcceptArgs {
    /// 元 inbox に来た `Move` activity 本文を保存した JSON ファイル。
    /// `actor` フィールドの URI が `signer` の lookup に使われる。
    /// `--signer` を併用するとそちらが優先。
    #[arg(long)]
    pub from: std::path::PathBuf,
    /// signer (= activity.actor) を上書きする `ActivityPub` actor URI。
    /// 通常は activity 本文の `actor` を信用してよいが、改竄を疑うときに使う。
    #[arg(long)]
    pub signer: Option<String>,
}
