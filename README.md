# Sakurasato 🌸

> **お一人様 TUI 専用 Fediverse サーバ — 端末の中の静かな隠れ家**

Sakurasato（さくらさと）は、利用者ただ一人のための ActivityPub サーバです。
Mastodon / Misskey / Pleroma 等の Fediverse 実装と連合しつつ、**Web の認証 UI を持たず**、操作はすべて **TUI クライアント** と **サーバ側 CLI** で行います。

## なぜ「お一人様」？

- 自分一人だけが投稿する Fediverse アカウントを、自分のサーバで持ちたい
- 公開アカウント運用のためのモデレーション機構（通報・凍結・複数ユーザ管理）は不要
- 結果として認証 UI も Web フロントエンドも要らず、**攻撃面が劇的に小さくなる**

## 主な特徴

| 領域 | 採用 |
| --- | --- |
| **TUI** | Kitty graphics protocol で画像表示・カスタム絵文字を表示。マウス（クリック / ホイール / ドラッグ）対応。カラースキーム切替可能 |
| **視覚刺激の制御** | アバター / 添付 / カスタム絵文字 / プレビュー / アニメ をそれぞれ on/off できる「視覚刺激抑制モード」 |
| **連合互換** | Mastodon 互換 + Misskey 絵文字リアクション (`EmojiReact`) + Misskey/Mastodon からの引っ越し (`Move` / `alsoKnownAs`) |
| **画像安全化** | EXIF・位置情報を含むメタデータを除去、WebP 再エンコードでステガノグラフィ的ペイロードも破壊。受理形式は **PNG / JPEG / WebP / GIF**（iOS の HEIC は非対応 → [DEPLOYMENT.md §6.3.3](DEPLOYMENT.md#633-mobile-クライアント側の設定) / [#263](https://github.com/nananek/sakurasato/issues/263)） |
| **隔離** | 画像デコードと外部 HTTP GET（リモートメディア / OGP / WebFinger）は **専用 media-proxy コンテナ** に隔離。`mem_limit` で OOM kill を本体に波及させない |
| **Docker** | `distroless` ベース + **rootless** + `read_only: true` + `cap_drop: [ALL]` + `no-new-privileges` |
| **言語** | Rust（メモリ安全 + `#![forbid(unsafe_code)]`）|

## クイックスタート（ghcr 公開イメージを使う場合）

```bash
git clone https://github.com/nananek/sakurasato.git
cd sakurasato

# シークレット生成（初回のみ）
chmod 700 secrets
[ -f secrets/postgres_password.txt ] || openssl rand -hex 32 > secrets/postgres_password.txt
[ -f secrets/s3_secret_key.txt ]    || openssl rand -hex 32 > secrets/s3_secret_key.txt
chmod 644 secrets/postgres_password.txt secrets/s3_secret_key.txt

# 公開ホスト名 / ユーザ名は env で上書き (docker-compose.override.yml に書く想定)
# 詳細は DEPLOYMENT.md §3.3 参照
#   SAKURASATO_SERVER__HOST: "sakurasato.example.com"
#   SAKURASATO_SERVER__USER: "me"

# ghcr 発行済みイメージを pull
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml pull

# 初期化（ローカル actor + RSA / Ed25519 鍵生成）
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server init

# 起動
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml up -d
```

**本番運用** (Cloudflare Tunnel 越しの公開 + Tailscale 経由の TUI アクセス) は [DEPLOYMENT.md](DEPLOYMENT.md) を、**TUI の操作方法** (起動 / キー操作 / コマンド一覧 / トラブルシュート) は [docs/TUI.md](docs/TUI.md) を、**サーバ CLI** (init / token / actor lock / follow-request / emoji import / move-out / miauth 等) のリファレンスは [docs/SERVER_CLI.md](docs/SERVER_CLI.md) を参照。Misskey 互換クライアント (Milktea / MissRirica など) からログインしたい場合の **MiAuth セットアップ** は [DEPLOYMENT.md §6](DEPLOYMENT.md#6-miauth-経路-mobile-misskey-互換) (デフォルト無効 / opt-in)。

## ローカル開発

```bash
# Rust ツールチェーン (rustup)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-unknown-linux-musl   # distroless 用静的ビルド

# sqlx-cli (マイグレーション + compile-time クエリ検証)
cargo install sqlx-cli --no-default-features --features postgres

# git hooks 有効化 (main ブランチ保護)
git config core.hooksPath .githooks

# 起動 (ローカル build + dev overlay でポート開放)
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build

# TUI (ホスト端末で別バイナリを直接実行)
cargo run -p sakurasato-tui
```

詳細な開発手順・コーディング規約は [CLAUDE.md](CLAUDE.md) を参照。

## アーキテクチャ

```
                ┌──────────── ホスト端末 ──────────────┐
                │  tui (Kitty 端末・マウス対応)        │
                │     │  Unix socket (REST + SSE)      │
                └─────┼────────────────────────────────┘
                      │ (Tailscale or ローカルマウント)
  ┌───────────────────┼──── docker-compose 内部ネット ────────┐
  │   ┌───────────┐   │   ┌──────────────┐   ┌──────────┐    │
  │   │  server   │───┴──▶│ media-proxy  │   │ postgres │    │
  │   │ (AP連合)  │◀─UDS─▶│ (隔離/変換)  │   │   :18    │    │
  │   │           │──S3──────────────────────────▶ versitygw │
  │   └─────┬─────┘       └──────┬───────┘                    │
  │      公開(443)            egress許可                       │
  └─────────┼──────────────────────────────────────────────────┘
            ▼ Cloudflare Tunnel (TLS 終端)
        Fediverse (Misskey / Mastodon / ...)
```

- **server** だけが外部公開。AP 配送と remote actor fetch のみ外向き HTTP を持つ。
- **media-proxy** は外部 GET（画像 / OGP / WebFinger）専用。本体 server は信頼できないバイト列をデコードしない。
- **postgres / versitygw** は内部ネットのみ。

詳細は [CLAUDE.md §3](CLAUDE.md#3-アーキテクチャ全体像) を参照。

## ドキュメント

| ファイル | 内容 |
| --- | --- |
| [CLAUDE.md](CLAUDE.md) | 設計方針・実装計画・コーディング規約・マイルストーン |
| [DEPLOYMENT.md](DEPLOYMENT.md) | 本番デプロイ手順（Cloudflare Tunnel + Tailscale + MiAuth opt-in） |
| [docs/TUI.md](docs/TUI.md) | TUI クライアント操作ガイド (起動 / キー操作 / コマンド一覧 / トラブルシュート) |
| [docs/SERVER_CLI.md](docs/SERVER_CLI.md) | サーバ CLI リファレンス (init / token / actor / follow-request / emoji import / move-out / miauth 等) |

## ロードマップ

実装ロードマップ M1〜M10 は **全完了** (2026-05-31)。詳細は [Issue #11](https://github.com/nananek/sakurasato/issues/11)。

## ライセンス

MIT © 2026 [nananek](https://github.com/nananek)

### サードパーティ

- TUI 絵文字検索の Unicode → shortcode マッピングに [github/gemoji](https://github.com/github/gemoji)
  (MIT © 2019 GitHub, Inc.) の `db/emoji.json` を `vendor/gemoji/` に同梱して使用しています。
  LICENSE 全文は [`vendor/gemoji/LICENSE`](vendor/gemoji/LICENSE) を参照。

## 名前の由来

神里綾華（原神）にちなむ。家名「神里（**Kamisato**）」の「**里（sato）**」に「**桜（sakura）**」を重ねた造語で、「**桜の里**」とも読めます。氷の静謐さがコンセプト（視覚刺激を抑えた落ち着き）とも響き合います。
