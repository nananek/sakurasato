//! Sakurasato TUI バイナリ。
//!
//! ホスト端末から `/run/sakurasato/local.sock` (= `config.server.local_api_socket`)
//! に Bearer 認証で接続し、タイムライン表示 / 投稿 / SSE 受信を行う。
//!
//! ## トークン
//!
//! `SAKURASATO_TOKEN` 環境変数を最優先、次に `--token-file <path>` で読む。
//! 直接 `--token` で渡すと履歴/プロセス一覧から漏れる可能性があるので
//! 警告だけ残して許可する (= 一旦動かしたい開発時の利便性のため)。

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use sakurasato_tui::TuiOptions;
use sakurasato_tui::client::Endpoint;
use sakurasato_tui::runtime;
use sakurasato_tui::suppression::ImageSuppression;
use sakurasato_tui::theme::Theme;
use tracing::warn;
use tracing_subscriber::EnvFilter;

/// Sakurasato TUI クライアント。
///
/// 視覚刺激抑制の 5 個別フラグ (`no_avatars` 等) で bool が 4 つ超えるが、
/// clap CLI ではフラグごとに 1 bool になるのが慣習なので明示的に許可する。
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI flag struct; bool は clap の慣習"
)]
#[derive(Debug, Parser)]
#[command(name = "sakurasato-tui", version)]
struct Cli {
    /// 接続先 Unix socket。既定は `config.server.local_api_socket` (= /run/sakurasato/local.sock)。
    /// `--api-url` と排他 ── 同時指定したら `--api-url` 優先。
    #[arg(long, env = "SAKURASATO_SOCKET")]
    socket: Option<PathBuf>,

    /// 接続先 TCP URL (例: `http://127.0.0.1:18080`)。Tailscale 経由で別端末
    /// から TUI を動かすときに使う (#69)。指定された場合は `--socket` を
    /// 無視して TCP モードで接続する。
    #[arg(long, env = "SAKURASATO_API_URL")]
    api_url: Option<String>,

    /// Bearer トークン。プロセス一覧から見えるので `--token-file` 推奨。
    #[arg(long, env = "SAKURASATO_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Bearer トークンを格納したファイル。改行は trim する。
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// テーマ名 (`sakura` / `dark` / `light`) または `--theme-file` 指定時のフォールバック。
    #[arg(long, default_value = "sakura")]
    theme: String,

    /// テーマ TOML を直接指定する (組み込みより優先)。
    #[arg(long)]
    theme_file: Option<PathBuf>,

    /// 1 ページの note 件数。1 〜 80。
    #[arg(long, default_value_t = 40)]
    page_size: i64,

    /// 画像表示を全要素一括で無効化する (= 視覚刺激抑制の killswitch)。
    /// Kitty 等の対応端末でも強制的にテキスト UI に倒す。
    /// 要素別に細かく切りたい場合は `--no-avatars` 等の個別フラグを使う
    /// (M9 PR2 で追加)。
    #[arg(long)]
    no_images: bool,

    /// アバター画像 (timeline 各 note の発信者アイコン) を抑制する。
    #[arg(long)]
    no_avatars: bool,

    /// 添付画像のサムネ表示を抑制する (将来 attachment 表示で参照)。
    #[arg(long)]
    no_attachments: bool,

    /// カスタム絵文字のインライン表示を抑制する (将来 emoji 表示で参照)。
    #[arg(long)]
    no_emojis: bool,

    /// ファイルピッカのローカル画像プレビューを抑制する。
    #[arg(long)]
    no_previews: bool,

    /// アニメ表示を抑制する (静的フレームのみ)。現状 ratatui-image は
    /// 1 フレーム目しか描かないため挙動上は同じだが、トグルとして残す。
    #[arg(long)]
    no_animations: bool,

    /// 起動せずに組み込みテーマ名を列挙して終了。
    #[arg(long)]
    list_themes: bool,

    /// TUI のログを追記するファイル。**未指定時は subscriber 自体を初期化
    /// しない** ため log は端末に流れない (= alt screen 上で warn!/error! が
    /// 描画と混ざらない)。デバッグ時はこの引数 + 別端末で `tail -f <path>`。
    #[arg(long, env = "SAKURASATO_LOG_FILE")]
    log_file: Option<PathBuf>,
}

fn resolve_suppression(cli: &Cli) -> ImageSuppression {
    if cli.no_images {
        return ImageSuppression::all_off();
    }
    let mut s = ImageSuppression::all_on();
    if cli.no_avatars {
        s.avatar = false;
    }
    if cli.no_attachments {
        s.attachment = false;
    }
    if cli.no_emojis {
        s.emoji = false;
    }
    if cli.no_previews {
        s.preview = false;
    }
    if cli.no_animations {
        s.animation = false;
    }
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_file.as_deref());

    if cli.list_themes {
        for name in Theme::builtin_names() {
            println!("{name}");
        }
        return Ok(());
    }

    let token = resolve_token(&cli)?;
    let endpoint = resolve_endpoint(&cli)?;

    let suppression = resolve_suppression(&cli);
    let theme = match cli.theme_file {
        Some(path) => Theme::from_path(&path)
            .with_context(|| format!("load theme file {}", path.display()))?,
        None => Theme::builtin(&cli.theme)
            .with_context(|| format!("load builtin theme `{}`", cli.theme))?,
    };
    let opts = TuiOptions {
        endpoint,
        token,
        theme,
        page_size: cli.page_size.clamp(1, 80),
        suppression,
    };

    runtime::run(opts).await
}

/// `--api-url` 優先、無ければ `--socket` → 既定パスへフォールバック。
/// `--api-url` は `http://host:port` を期待する (末尾 `/` は削る)。
fn resolve_endpoint(cli: &Cli) -> anyhow::Result<Endpoint> {
    if let Some(raw) = &cli.api_url {
        let base = raw.trim_end_matches('/').to_string();
        // **[PR #70 review medium]**: `https://` は明示エラー。hyper-util の
        // `build_http()` は TLS 非対応で、無音で「平文 https 試行 → connection
        // error」になる footgun を防ぐ。Tailscale tailnet (= WireGuard) で
        // 暗号化される前提なので、TUI 側で TLS スタックを抱えない設計。
        if base.starts_with("https://") {
            anyhow::bail!(
                "--api-url https:// is not supported (TUI に TLS スタック無し)。\
                 Tailscale tailnet 内の http://<host>.<tailnet>.ts.net:<port> を指定し、\
                 暗号化は WireGuard 層に任せてください"
            );
        }
        if !base.starts_with("http://") {
            anyhow::bail!("--api-url must start with http:// (got {raw:?})");
        }
        return Ok(Endpoint::Tcp { base });
    }
    let socket = cli.socket.clone().unwrap_or_else(default_socket_path);
    Ok(Endpoint::Unix(socket))
}

fn resolve_token(cli: &Cli) -> anyhow::Result<String> {
    if let Some(path) = &cli.token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!("token file {} is empty", path.display());
        }
        return Ok(trimmed.to_string());
    }
    if let Some(token) = &cli.token {
        // env 経由なら警告しない。`--token <値>` で渡したケースだけ警告する。
        let from_env = std::env::var("SAKURASATO_TOKEN").ok().as_deref() == Some(token.as_str());
        if !from_env {
            warn!("--token は ps から見えるおそれあり。--token-file 推奨");
        }
        return Ok(token.clone());
    }
    anyhow::bail!(
        "no token provided. set SAKURASATO_TOKEN, --token-file <path>, or --token <value>"
    )
}

fn default_socket_path() -> PathBuf {
    PathBuf::from("/run/sakurasato/local.sock")
}

fn init_tracing(log_file: Option<&std::path::Path>) {
    // 端末を独占するので tracing を stderr に出すと alt screen の描画と
    // 同じ pty 上で混ざる ── ratatui の次フレームで上書きされるまで残骸が
    // 視える (= warn! / error! が出るたび UI が "流れる")。
    //
    // `--log-file <path>` (`SAKURASATO_LOG_FILE`) が指定されたときだけ
    // ファイルへ append する。未指定なら subscriber を init しない (=
    // イベントは全部捨てられる)。デバッグ時は path 指定 + 別端末で
    // `tail -f <path>` 想定。
    //
    // フィルタは `SAKURASATO_LOG` → `RUST_LOG` → `warn` の順で fallback。
    let Some(path) = log_file else {
        return;
    };
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(f) => f,
        Err(err) => {
            // alt screen に入る前なので 1 行だけ stderr に吐いて諦める。
            eprintln!(
                "sakurasato-tui: failed to open log file {}: {err}",
                path.display(),
            );
            return;
        }
    };
    let filter = EnvFilter::try_from_env("SAKURASATO_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_with(api_url: Option<&str>) -> Cli {
        Cli {
            socket: None,
            api_url: api_url.map(str::to_string),
            token: None,
            token_file: None,
            theme: "sakura".into(),
            theme_file: None,
            page_size: 40,
            no_images: false,
            no_avatars: false,
            no_attachments: false,
            no_emojis: false,
            no_previews: false,
            no_animations: false,
            list_themes: false,
            log_file: None,
        }
    }

    #[test]
    fn resolve_endpoint_accepts_http() {
        let ep = resolve_endpoint(&cli_with(Some("http://127.0.0.1:18080/"))).unwrap();
        match ep {
            Endpoint::Tcp { base } => assert_eq!(base, "http://127.0.0.1:18080"),
            Endpoint::Unix(_) => panic!("expected Tcp"),
        }
    }

    #[test]
    fn resolve_endpoint_rejects_https_with_explanation() {
        let err = resolve_endpoint(&cli_with(Some("https://example.test"))).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("https://"), "msg: {msg}");
        assert!(msg.contains("TLS"), "msg: {msg}");
    }

    #[test]
    fn resolve_endpoint_rejects_other_schemes() {
        let err = resolve_endpoint(&cli_with(Some("ws://example.test"))).unwrap_err();
        assert!(format!("{err}").contains("http://"));
    }

    #[test]
    fn resolve_endpoint_falls_back_to_socket() {
        let ep = resolve_endpoint(&cli_with(None)).unwrap();
        match ep {
            Endpoint::Unix(p) => assert_eq!(p, default_socket_path()),
            Endpoint::Tcp { .. } => panic!("expected Unix"),
        }
    }
}
