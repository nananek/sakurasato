//! Sakurasato TUI クライアント (M5)。
//!
//! ホスト端末で動作し、`server` クレートが Unix domain socket 上に提供する
//! `/api/v1/*` を Bearer 認証で叩く。連合プロトコルや DB は直接触らない:
//!
//! ```text
//!   tui (ratatui)
//!     │ HTTP/1.1 over UDS  (Bearer <token>)
//!     ▼
//!   /run/sakurasato/local.sock
//!     │
//!     ▼
//!   server::local_api::router (whoami / timeline / notes / stream)
//! ```
//!
//! ## ライブラリとして公開する理由
//!
//! `tests/` から theme / client / app の各サブモジュールを叩けるようにする。
//! バイナリ (`main.rs`) は CLI 解析と `runtime::run` の呼び出しだけ。
//!
//! ## モジュール構成
//!
//! - [`theme`] ── `config/themes/*.toml` の読み込みと `ratatui::style::Color`
//!   への変換。組み込みテーマ 3 種 (sakura/dark/light) を `include_str!` で
//!   バンドルしてあるので、設定無しでも必ず起動できる。
//! - [`client`] ── Unix socket 越しに `/api/v1/{whoami,timeline/home,notes}`
//!   を叩く HTTP/1.1 クライアント。`hyper-util` の legacy client + `hyperlocal::UnixConnector`。
//! - [`sse`] ── `/api/v1/stream` を購読し、`TimelineEvent` を tokio channel に
//!   流すバックグラウンドタスク。`eventsource-stream` でフレームを解く。
//! - [`app`] ── アプリ状態 (タイムライン Vec、選択 index、compose buffer、focus)。
//! - [`event`] ── crossterm の `Event` を `Action` enum にマップする層。
//!   キー/マウスの解釈をここに集中させて、`runtime` 側を薄く保つ。
//! - [`runtime`] ── ratatui ループ。input task / sse task / render を `tokio::select!`
//!   で回し、`Action` を `App` に適用する。
//! - [`ui`] ── 各パネル (timeline / compose / status) の描画関数。色は必ず
//!   [`theme::Theme`] 経由 (`pedantic + workspace lints` で色のハードコードを
//!   弾く慣習に従う)。

#![forbid(unsafe_code)]

pub mod app;
pub mod client;
pub mod compose;
pub mod event;
pub mod image_cache;
pub mod picker;
pub mod preview;
pub mod reaction_prompt;
pub mod runtime;
pub mod sse;
pub mod theme;
pub mod ui;

/// バイナリの runtime 設定。CLI から組み立てて [`runtime::run`] に渡す。
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// 接続先の Unix socket。`config/default.toml` の `server.local_api_socket` と合わせるのが既定。
    pub socket: std::path::PathBuf,
    /// Bearer トークン。`sakurasato-server token issue` で発行されたもの。
    pub token: String,
    /// テーマ名 (`sakura`/`dark`/`light` の組み込み、または `--theme-file` で
    /// 直接指定したパスの拡張子抜きファイル名)。
    pub theme: theme::Theme,
    /// 1 ページ分のタイムライン取得件数。`/api/v1/timeline/home?limit=` に渡す。
    pub page_size: i64,
    /// 画像 (アバター) 表示を有効化する。`false` または terminal が
    /// 画像プロトコル非対応のときはテキスト専用にフォールバックする。
    /// M9 で要素別 (添付 / 絵文字 / プレビュー / アバター) の細粒度トグルに
    /// 拡張する予定だが、PR2 では単一ブール。
    pub images_enabled: bool,
}
