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
    /// Flush a single outbound delivery from the queue (運用 retry 用)。
    ///
    /// 配送 worker は通常自動で retry を回すが、以下のケースで個別 flush が要る:
    /// - **Issue #113 経由**: 既存 pending follow が ある状態で再度 follow を
    ///   叩いたとき、本 CLI は重複 enqueue を抑止して "no-op" 返却する。
    ///   元の `delivery_queue` 行が失敗していたら本コマンドで明示再送する。
    /// - worker の retry 上限を使い切って `failed` で諦めた行を再投入する
    ///   (= 相手側障害が長期化した場合)。
    ///
    /// `queue_id` は `psql` で `SELECT id, status, last_error FROM delivery_queue
    /// WHERE status IN ('pending','failed') ORDER BY id DESC LIMIT 20;` 等で
    /// 列挙する。本 CLI は意図的に list 機能を持たない (= 攻撃面を増やさない)。
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
    /// Manage local actor state (Issue #66 — 鍵アカ運用 lock/unlock)。
    ///
    /// `actor lock` / `actor unlock` で `manuallyApprovesFollowers` を切替える。
    /// 切替後は actor `Update` activity をフォロワー全員に配信する (= プロ
    /// フィール変更と同じ作法。受信側のキャッシュを更新させる)。
    Actor(ActorArgs),
    /// Manage incoming follow requests when locked (Issue #66)。
    ///
    /// `manually_approves_followers = TRUE` の local actor では inbound Follow
    /// が `pending` で据え置かれる。`follow-request list/approve/reject` で
    /// 承認・拒否を行う。approve は Accept activity を `delivery_queue` に
    /// 積み、state を `accepted` に遷移。reject は Reject activity を積み、
    /// state を `rejected` に遷移。
    FollowRequest(FollowRequestArgs),
    /// Manage Discord 互換 webhook 通知チャンネル。
    ///
    /// 受信 (mention / DM / quote / reaction / renote / follow / follow-request)
    /// を Discord (および Slack / Misskey 互換 webhook) に push 通知する宛先を
    /// 管理する。お一人様サーバには Web UI が無いので「自分宛の何かが来た」
    /// ことを外部で気付く経路として用意している。
    ///
    /// 実 POST は既存 `delivery_queue` 経由 (= retry / backoff / dead 状態機械
    /// を AP 配送と共有)。delivery worker は `activity.type` が `Webhook:`
    /// prefix の行に対しては HTTP 署名を skip して `application/json` で
    /// `payload` を送る。
    NotificationChannel(NotificationChannelArgs),
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
    /// 鍵アカ (`manuallyApprovesFollowers = true`) として initialise する
    /// (Issue #66 / M12)。`--force` 経路では既存 lock 状態は **保たれ** て
    /// いる ── `--locked` を渡せば lock を維持/有効化、渡さなくても既存が
    /// lock なら lock のまま。lock を解除したいときは init 後に
    /// `sakurasato-server actor unlock` を叩く。
    #[arg(long, default_value_t = false)]
    pub locked: bool,
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
    /// 生トークンを stdout に出す代わりに **指定ファイルだけ** に書き込む。
    /// 既存ファイルは上書きせず失敗する (= 古いトークンが意図せず奪われる
    /// のを避ける)。compose の名前付きボリューム経由でテストランナや TUI
    /// に共有する用途を想定。
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,
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
    ///
    /// `--actor-uri` 指定時のみ省略可能 (`required_unless_present`)。
    /// **`default_value` は付けない**: clap v4 で `default_value` が設定された
    /// 引数は常に「present」扱いになり、`required_unless_present` の発火が
    /// 環境依存で不安定になる ([[m10-pr2-review]] 指摘 #1 対応)。
    /// `Option<String>` で受けて follow.rs 側で `unwrap_or_default()` する。
    #[arg(required_unless_present = "actor_uri")]
    pub acct: Option<String>,
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

#[derive(Debug, Args)]
pub struct ActorArgs {
    #[command(subcommand)]
    pub command: ActorCommand,
}

#[derive(Debug, Subcommand)]
pub enum ActorCommand {
    /// 鍵アカ化 (`manuallyApprovesFollowers = true`)。Update activity を
    /// フォロワーに配信して相手側のキャッシュを更新する。
    Lock,
    /// 鍵アカ解除 (`manuallyApprovesFollowers = false`)。同じく Update を
    /// フォロワーに配信。lock 中に溜まった pending Follow は手動で
    /// `follow-request approve/reject` する必要がある (= unlock しただけで
    /// 過去の pending が自動 accept されるわけではない: 鍵 ON 中に届いた
    /// 「待ち」を unlock の事故で全部 accept してしまうのを避ける設計)。
    Unlock,
}

#[derive(Debug, Args)]
pub struct FollowRequestArgs {
    #[command(subcommand)]
    pub command: FollowRequestCommand,
}

#[derive(Debug, Subcommand)]
pub enum FollowRequestCommand {
    /// `follow.state = 'pending'` かつ followed が local actor の Follow
    /// を列挙する。`id` / `follower` (`ap_id`) / `received_at` を表示。
    List,
    /// `--id <N>` で指定した pending Follow を承認し、Accept activity を
    /// `delivery_queue` に積む + `follow.state = 'accepted'` に遷移する。
    Approve(FollowRequestMutateArgs),
    /// `--id <N>` で指定した pending Follow を拒否し、Reject activity を
    /// `delivery_queue` に積む + `follow.state = 'rejected'` に遷移する。
    Reject(FollowRequestMutateArgs),
}

#[derive(Debug, Args)]
pub struct FollowRequestMutateArgs {
    /// `follow.id` (= `follow-request list` で表示される number)。
    #[arg(long)]
    pub id: i64,
}

#[derive(Debug, Args)]
pub struct NotificationChannelArgs {
    #[command(subcommand)]
    pub command: NotificationChannelCommand,
}

#[derive(Debug, Subcommand)]
pub enum NotificationChannelCommand {
    /// Webhook URL を登録する。`url` は登録時に SSRF ガード
    /// ([`crate::net_guard::host_blocked`]) で検査するが、配送時にも再検査する
    /// (DNS 再解決後の TOCTOU 防御)。`name` は CLI / 表示用ラベルで UNIQUE。
    Add(NotificationChannelAddArgs),
    /// 登録チャンネルを `id`/`name`/`format`/`url ホスト`/`on の event`
    /// の 1 行で列挙する。**URL 全体は表示しない** (URL が漏れれば第三者が
    /// 同じチャンネルに任意メッセージを撃てるため)。
    List,
    /// `--id N` でハード削除する。
    Remove(NotificationChannelIdArgs),
    /// `notify_<event>` を **ON** に設定する (idempotent)。`--event all` は
    /// 7 個の `notify_*` を一斉 TRUE にする (= チャンネル全 event 有効化)。
    Enable(NotificationChannelEventArgs),
    /// `notify_<event>` を **OFF** に設定する (idempotent)。`--event all` は
    /// 7 個の `notify_*` を一斉 FALSE にする (= チャンネル全停止)。
    Disable(NotificationChannelEventArgs),
    /// テスト通知 (固定文言の embed/plain) を 1 件 `delivery_queue` に enqueue
    /// する。worker が拾って実 POST する (即時送出は worker のティック次第)。
    Test(NotificationChannelIdArgs),
}

#[derive(Debug, Args)]
pub struct NotificationChannelAddArgs {
    /// 表示・CLI 識別用のラベル (例: "discord-personal")。
    #[arg(long)]
    pub name: String,
    /// Webhook URL。public IP / 公開ドメインのみ。private / loopback /
    /// link-local / reserved は SSRF ガードで遮断される。
    #[arg(long)]
    pub url: String,
    /// `embed` (Discord embed JSON、既定) または `plain` (Slack / Misskey
    /// 互換 fallback の `{"content": "..."}`)。
    #[arg(long, default_value = "embed")]
    pub format: String,
}

#[derive(Debug, Args)]
pub struct NotificationChannelIdArgs {
    #[arg(long)]
    pub id: i64,
}

/// `enable` / `disable` の共通引数。`--id` でチャンネル、`--event` で対象。
#[derive(Debug, Args)]
pub struct NotificationChannelEventArgs {
    #[arg(long)]
    pub id: i64,
    /// `all` / `mention` / `direct` / `quote` / `reaction` / `renote` /
    /// `follow` / `follow-request` のいずれか。`all` は 7 個の `notify_*`
    /// を一斉セット (= チャンネル全 event 有効化 / 全停止)。
    #[arg(long)]
    pub event: String,
}
