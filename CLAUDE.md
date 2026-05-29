# Sakurasato 🌸 — お一人様 TUI 専用 Fediverse サーバ

> このファイルは Claude Code 向けのプロジェクトガイド兼実装計画書です。
> 作業の前に必ずここを参照し、設計方針・規約・マイルストーンに従ってください。

---

## 1. プロジェクト概要

**Sakurasato** は、利用者ただ一人のための ActivityPub（Fediverse）サーバ。
コンセプトは「**端末の中の静かな隠れ家**」。

- **利用者はマジで一人だけ。** Web の認証 UI は持たない。管理・認証はすべて**サーバ側 CLI** で行う。
- **操作は TUI 専用。** ただし Kitty graphics protocol で画像をきちんと見られ、カスタム絵文字の応酬もできる。同時に **視覚刺激を抑えるモード**（要素別の画像 on/off）を備える。**マウス操作にも対応**する。
- **セキュリティ最重視。** 実装言語は **Rust**。画像デコードや外部 URL 取得という「危険な処理」は本体から**隔離コンテナ**に追い出し、最悪 OOM kill で済むようにする。Docker は **distroless + rootless** を徹底。
- ストレージは **PostgreSQL 18** と **S3 互換（versitygw）** が必須。versitygw は公開せず、保存先プロトコルとしてのみ使う。
- **Misskey 形式 zip での絵文字インポート**、**Misskey 絵文字リアクション**、**Misskey/Mastodon からの引っ越し（`Move`）受け入れ**に対応する。
- Web UI は最小限（パーマリンク等、連合に必要なものだけ）。

### 名前の由来
神里綾華（原神）にちなむ。家名「神里（**Kamisato**）」の「**里（sato）**」に「**桜（sakura）**」を重ねた造語で、「**桜の里**」とも読める。氷の静謐さがコンセプト（視覚刺激を抑えた落ち着き）とも響き合う。

### 参考構成
`nekonoverse/nekonoverse`（GitHub, Python/FastAPI 製 AP サーバ）の **media-proxy 隔離設計**を踏襲する——
画像変換専用コンテナ（Unix ソケットのみ・外部アクセス不可・メモリ上限）＋ SSRF 対策付き summary/OGP 取得コンテナ。

---

## 2. 確定した技術選定

| 項目 | 決定 | 補足 |
|---|---|---|
| 言語 | **Rust** | メモリ安全 + GC なし。C++ よりセキュリティ方針に合致 |
| 連合互換 | **Mastodon 互換 + Misskey 絵文字リアクション/Move** | 連合プロトコル層での相互運用性を最重視 |
| アーキ | **サーバ常駐デーモン + TUI クライアント分離** | TUI はローカル API（Unix ソケット）で接続 |
| DB | **PostgreSQL 18** | CockroachDB は単一ノード用途ではオーバーキルで不利 |
| ストレージ | **versitygw（S3 互換）** | 非公開・内部ネットのみ。POSIX volume バックエンド |
| Docker | **distroless + rootless 徹底** | read-only rootfs / cap drop ALL / no-new-privileges |
| TUI 描画 | **ratatui + ratatui-image + crossterm** | Kitty 画像（自動検出）・マウス対応 |

**主要クレート**: `axum 0.8`, `activitypub_federation`(LemmyNet), `sqlx 0.8`(postgres, compile-time checked), `tokio`, `aws-sdk-s3`, `clap`(derive), `ratatui` / `ratatui-image` / `crossterm`, `image`(変換), `serde`/`serde_json`, `reqwest`(media-proxy), `tracing`(ログ)。

> 注: `activitypub_federation` は安定線 0.6.x。0.7.0-beta は axum 0.8 対応かつ `ActivityHandler`→`Activity` 改名あり。採用バージョンは実装着手時に確認すること。

---

## 3. アーキテクチャ全体像

```
                ┌─────────────────────── ホスト端末 ───────────────────────┐
                │  tui (別バイナリ・Kitty端末)                              │
                │     │  Unix ドメインソケット (REST + SSE)                 │
                └─────┼─────────────────────────────────────────────────────┘
                      │ (ソケットをホストにマウント)
  ┌───────────────────┼──────────────── docker-compose 内部ネット ──────────────────┐
  │   ┌───────────┐   │   ┌──────────────┐        ┌────────────┐   ┌──────────────┐  │
  │   │  server   │───┴──▶│ media-proxy  │        │  postgres  │   │  versitygw   │  │
  │   │ (AP連合)   │◀─UDS─▶│ (隔離/変換)   │        │   :18      │   │  (S3互換)     │  │
  │   │           │──S3──────────────────────────────────────────▶ │              │  │
  │   │           │──SQL─▶│              │        └────────────┘   └──────────────┘  │
  │   └─────┬─────┘       └──────┬───────┘                                            │
  │      公開(443)            egress許可                                              │
  └─────────┼──────────────────────────────────────────────────────────────────────┘
            ▼ TLS終端 (caddy 等, 任意)
        Fediverse (Misskey / Mastodon / ...)
```

- **server だけが外部公開**（リバースプロキシ経由で 443）。
- **外部への egress（リモートメディア取得・OGP）は media-proxy のみに許可**。
- postgres / versitygw は内部ネットのみ。TUI はコンテナ外でホスト端末から Unix ソケット接続。

---

## 4. リポジトリ構成（Cargo workspace）

```
sakurasato/
├─ Cargo.toml                 # workspace
├─ CLAUDE.md                  # 本ファイル
├─ crates/
│  ├─ core/                   # ドメイン型・AP 型・設定(toml)・共有ロジック
│  ├─ server/                 # AP 連合デーモン + ローカル API + 最小 Web + 管理 CLI
│  ├─ tui/                    # TUI クライアント（別バイナリ。ホスト端末で実行）
│  └─ media-proxy/            # 隔離コンテナ：リモート取得 / 画像変換 / OGP取得 / アップロードサニタイズ
├─ migrations/                # sqlx マイグレーション
├─ docker/                    # 各 Dockerfile（distroless/rootless）
│  ├─ server.Dockerfile
│  └─ media-proxy.Dockerfile
├─ docker-compose.yml         # 本番想定
├─ docker-compose.dev.yml     # 開発用（ローカルポート公開など）
└─ config/
   ├─ default.toml            # 既定設定
   └─ themes/                 # カラースキーム(*.toml)
```

---

## 5. コンポーネント設計

### 5.1 server（AP 連合デーモン）
- **インバウンド HTTP**:
  - `/.well-known/webfinger`, `/.well-known/nodeinfo`, `/nodeinfo/2.1`
  - `/users/<name>`（actor JSON）, `/inbox`（共有 inbox）, `/users/<name>/inbox`, `/users/<name>/outbox`
  - HTTP 署名検証は `activitypub_federation` に委譲。
- **アウトバウンド**: 配送キューを Postgres に永続化し、tokio ワーカーで**指数バックオフ・リトライ**送出。
- **対応アクティビティ**: `Create`/`Note`, `Follow`/`Accept`/`Reject`, `Like`, `EmojiReact`(Misskey 拡張), `Announce`, `Update`, `Delete`, `Undo`, **`Move`(`movedTo`/`alsoKnownAs`)**。
- **Actor 構成**: 単一ユーザー actor ＋ `instance.actor`(application actor, 必要に応じて生成)。
- **ローカル API（server ⇄ tui）**: Unix ドメインソケット上の REST + SSE（タイムライン購読）。お一人様前提でソケットのファイルパーミッションが認証境界。トークン発行は CLI から可能。**メディアアップロード**エンドポイント（アイコン/ヘッダ/添付）を持ち、受領後 media-proxy でサニタイズ・変換 → versitygw 格納 → メタデータを DB 登録。
- **最小 Web UI**: 投稿のパーマリンク（AP Note を人間可読 HTML で）、WebFinger/NodeInfo/actor JSON、メディア配信エンドポイント `GET /media/<key>`（versitygw から取得して配信。versitygw 自体は非公開）。
- **管理 CLI**（同バイナリのサブコマンド, `clap`）: `init`（ユーザー/鍵生成）, `emoji import <zip>`, `follow <acct>`, `move accept`, `token issue` など。**Web 認証 UI は作らない。**

### 5.2 tui（TUI クライアント・別バイナリ）
- ホスト端末で動作し、server のローカル API（Unix ソケット）へ接続。
- **画像**: `ratatui-image` で Kitty graphics protocol（Sixel/iTerm2 もフォールバック対応、自動検出）。アバター・添付画像・カスタム絵文字を表示。
- **マウス**: `crossterm` のマウスイベント（クリック・ホイール・ドラッグ）をハンドリング。
- **カラースキーム**: `config/themes/*.toml` でパレット定義。組み込みテーマ複数 ＋ ユーザー定義を切替可能（**最初からテーマ抽象を入れる**。色をハードコードしない）。
- **視覚刺激抑制モード**: 要素別（アバター / 添付 / カスタム絵文字 / プレビュー / アニメ）に画像表示 on/off。アニメは静的フレーム化。テーマと統合。
- **ファイルセレクタ + プレビュー + アップロード（必須）**: アイコン・ヘッダ画像・投稿添付画像は外向きに重要なため、TUI 内にローカルファイルブラウザ（パス入力/補完つき）を持ち、選択画像を `ratatui-image` でプレビューしてからアップロードする。アップロードはローカル API 経由で server → media-proxy（サニタイズ/変換）→ versitygw 格納まで通し、アイコン/ヘッダは actor 更新（`Update`）として連合送出、添付は投稿に紐付ける。アップロード進捗・失敗を TUI で表示。

### 5.3 media-proxy（隔離コンテナ）
- **役割**:
  1. リモートメディア取得（**SSRF 対策**: private/loopback/link-local/reserved を遮断、リダイレクト毎に再検証、許可 CIDR allowlist）
  2. 画像変換（リサイズ/WebP 化, `max_size`/`max_pixels` 上限。アバター/絵文字/プレビュー等のバリアント生成）
  3. OGP/summary 取得
  4. **アップロード画像のサニタイズ**（再エンコードで埋め込みペイロード除去・EXIF/位置情報などメタデータ除去）
- **本体は生ファイルをデコードしない。** 必ず media-proxy 経由でサニタイズ済みデータのみ扱う。
- **隔離**: 本体とは Unix ソケット（または内部ネットのみ）で通信し、外部公開なし。**egress を許可するのはこのコンテナだけ**。`mem_limit` を設定し、信頼できない入力のデコードが暴走しても OOM kill で本体に波及させない。
- nekonoverse の `media-proxy-rs`（変換専用・ソケット限定・512M）と `summary-proxy`（SSRF 対策）の役割を Rust の一コンテナに統合。必要なら変換と取得をさらに分割可能な構成にしておく。

### 5.4 絵文字（Misskey 形式 zip インポート）
- **zip 構造**: トップレベル `meta.json`（`metaVersion`/`host`/`exportedAt`/`emojis[]`）＋画像ファイル。
- 各 `emojis[]`: `downloaded`(真のみ取込), `fileName`(zip 内画像名), `emoji`{ `name`(=`:shortcode:`), `category`, `aliases`, ... }。
- `downloaded == true` のものだけ取り込み、`name`(shortcode)・`category`・`aliases` を DB 登録、`fileName` の画像を versitygw へ格納。**同名は上書き**。
- CLI: `sakurasato emoji import <file.zip>`。カスタム絵文字は連合送受信し、TUI で Kitty 表示。

---

## 6. docker-compose 構成

| service | image 方針 | 公開 | 備考 |
|---|---|---|---|
| `postgres` | `postgres:18-alpine` | 内部のみ | データ volume |
| `versitygw` | `versity/versitygw` | 内部のみ | POSIX volume バックエンド、S3 プロトコル提供 |
| `media-proxy` | distroless/static（自作） | 内部のみ・**egress 許可** | `mem_limit` 設定、本体とソケット通信 |
| `server` | distroless/static（自作） | リバースプロキシ経由で外部 | TUI 用 Unix ソケットをホストへマウント |
| `proxy`(任意) | caddy 等 | 443 | 連合に必須の TLS 終端 |

TUI クライアントはコンテナ外（ホスト端末）で実行し、マウントされた Unix ソケットへ接続。

---

## 7. セキュリティ方針（全コンテナ共通・厳守）

- **distroless ベース**: Rust 静的バイナリ（`x86_64-unknown-linux-musl` 等）→ `gcr.io/distroless/static` か `scratch`。マルチステージビルド。
- **rootless**: 非 root UID で実行（`USER nonroot` / 数値 UID）。
- `read_only: true`（root fs）＋ 必要箇所のみ `tmpfs`(/tmp)。
- `cap_drop: [ALL]`、`security_opt: ["no-new-privileges:true"]`。
- **ネットワーク分離**: media-proxy 以外の egress を絞る（internal network）。versitygw・postgres は内部ネットのみ。
- **シークレット**: compose secrets / 環境変数（postgres・S3 認証）。HTTP 署名鍵は CLI 生成で安全に保管（DB 内 or マウントした鍵ファイル、パーミッション 600）。
- **危険な入力の隔離**: 画像デコード・外部 URL 取得は必ず media-proxy 側。server は信頼できないバイト列を直接デコードしない。

---

## 8. 開発セットアップ

> 現状この環境に Rust ツールチェーンは未インストール。最初に用意する。

```bash
# Rust（rustup 経由）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-unknown-linux-musl   # distroless 用静的ビルド

# sqlx-cli（マイグレーション・compile-time クエリ検証）
cargo install sqlx-cli --no-default-features --features postgres

# docker / docker compose は導入済み（v29 / compose v5）

# git hooks を有効化（main 保護: 直接 commit/push を禁止）
git config core.hooksPath .githooks
```

---

## 9. ビルド・実行コマンド（確立後に追記）

```bash
# 開発
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# DB マイグレーション
sqlx migrate run

# 起動（開発）
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build
# server 管理 CLI
cargo run -p sakurasato-server -- init
# TUI（ホスト端末）
cargo run -p sakurasato-tui
```

---

## 10. コーディング規約

- **`unsafe` 禁止**（やむを得ない場合は理由をコメント＋レビュー必須）。クレートに `#![forbid(unsafe_code)]` を付与。
- エラーは `thiserror`（ライブラリ）/ `anyhow`（バイナリ境界）で扱い、握り潰さない。
- `sqlx` は **compile-time checked query**（`query!`/`query_as!`）を基本にする。
- TUI の色は**必ずテーマ経由**。ハードコード禁止。
- 外部から来るバイト列（画像・HTTP レスポンス）を server 本体でデコードしない。media-proxy に委譲。
- ログは `tracing`。秘密情報・トークンはログに出さない。
- 既存コードのスタイル（命名・コメント密度・イディオム）に合わせる。

---

## 11. リポジトリ運用・CI/CD

### リポジトリ
- GitHub: **`nananek/sakurasato`**（Public / MIT / © 2026 nananek）
- 実装ロードマップ: Issue **#1〜#10**（= M1〜M10）、**#11** がトラッキング。

### ブランチ運用
- **`main`**: リリース専用の保護ブランチ。**直接 push 禁止（PR 必須）**、force-push/削除禁止、linear history。
- **`develop`**: 開発の主軸。Claude はここで自由に作業・直 push してよい。
- フロー: `develop`（必要なら `feature/*` → `develop`）→ **`develop` → `main` の PR** → **nananek が手動マージ**。
  - **Claude は `develop` → `main` のマージを実行しない**（auto-merge も無効化済み）。PR 作成までが Claude の役割。

### リリース（バージョニング）
- **CalVer `YYYY.MM.patch`**（Misskey 準拠、例 `2026.05.0`）。
- `develop` → `main` を手動マージした後、`main` にタグ `YYYY.MM.patch` を打ってリリース。**タグ push は許可**。

### コミット規約
- **Conventional Commits**。type 例: `feat` / `fix` / `docs` / `refactor` / `test` / `perf` / `chore` / `ci` / `docker` / `deps`。
- 1 行目: `type(scope): 要約`。本文に理由。Claude 作業分は末尾に `Co-Authored-By: Claude ...`。

### git hooks（`.githooks/`, `core.hooksPath` で有効化）
- **`pre-push`**: `main` への直接 push を拒否（**タグ push と develop は許可**）。
- **`pre-commit`**: `main` ブランチ上での直接コミットを拒否。
- 有効化（クローン後に一度）: `git config core.hooksPath .githooks`。リモートの branch protection と二重で守る。

### CI / 自動化（`.github/`）
- **CI** (`ci.yml`): `cargo fmt --check` / `clippy -D warnings` / `test`。`main`・`develop` の push と PR。required status check = `ci`。`Cargo.toml` が無い間はスキップして緑（M1 で本稼働）。
- **CodeQL** (`codeql.yml`): Rust SAST（`build-mode: none`）。`.rs`/`Cargo.*` 変更時と週次。
- **Dependency Review** (`dependency-review.yml`): high 以上で fail、GPL/AGPL/SSPL を deny（MIT 維持）。
- **Claude PR レビュー** (`claude-review.yml`): `anthropics/claude-code-action@v1`、認証 **`secrets.CLAUDE_CODE_OAUTH_TOKEN`**。PR 自動 + `@claude` メンション、verdict 付き top-level コメントを必ず投稿。
- **Dependabot** (`dependabot.yml`): `cargo`/`github-actions`/`docker` を週次更新。
- **アラート**: Dependabot alerts / 自動セキュリティ修正 / secret scanning + push protection 有効化済み。

### 要設定の secret
- **`CLAUDE_CODE_OAUTH_TOKEN`** — Claude PR レビュー用。`gh secret set CLAUDE_CODE_OAUTH_TOKEN --repo nananek/sakurasato`（iikanji と同じ値でOK）。

---

## 12. 実装マイルストーン

1. workspace 初期化、`docker-compose` 雛形、distroless/rootless Dockerfile、設定ローダ。
2. `sqlx` マイグレーション（actor / note / follow / delivery_queue / emoji / reaction）、core 型。
3. AP 連合の骨格: actor / webfinger / nodeinfo / inbox / outbox、HTTP 署名、`Follow`/`Accept`、`Create`/`Note` 送受信＋配送キュー。
4. ローカル API（Unix ソケット REST + SSE）＋最小 Web（パーマリンク・メディア配信）。
5. TUI 基礎: タイムライン・投稿・`ratatui-image`・マウス・カラースキーム切替。
6. media-proxy 隔離コンテナ: 取得（SSRF 対策）/ 変換 / OGP / アップロードサニタイズ、`mem_limit`・ソケット限定。
7. TUI のファイルセレクタ + プレビュー + アップロード（アイコン/ヘッダ/添付）、actor `Update` 連合。
8. カスタム絵文字 + Misskey zip インポート CLI + `EmojiReact` 連合。
9. `Move`（引っ越し）受け入れ/送出、視覚刺激抑制モード（要素別トグル）仕上げ。
10. セキュリティ硬化（read-only / cap drop / egress 制限）と管理 CLI 整備。

---

## 13. 検証（エンドツーエンド）

- `docker compose up` で postgres / versitygw / media-proxy / server が **非 root・read-only fs** で起動することを確認。
- WebFinger 解決: `curl 'https://<host>/.well-known/webfinger?resource=acct:<user>@<host>'` が actor を返す。
- 実 Misskey/Mastodon インスタンスとの相互フォロー・投稿・絵文字リアクションを確認（nekonoverse のローカル federation テスト構成を参考にローカルで対向）。
- TUI を Kitty 端末で起動 → 画像表示 / マウス操作 / テーマ切替 / 要素別画像トグルを確認。
- `sakurasato emoji import <misskey.zip>` → TUI でカスタム絵文字表示を確認。
- TUI のファイルセレクタで画像を選択 → プレビュー → アイコン/ヘッダ/添付としてアップロードし、media-proxy でサニタイズ（EXIF 除去）された画像が versitygw に格納され、連合・パーマリンクに反映されることを確認。
- テストアカウントから `Move` を送り、フォロワー引き継ぎ受け入れを確認。
- egress 制限の確認: server コンテナから外部 URL 取得が直接できず、media-proxy 経由のみで成立すること。
