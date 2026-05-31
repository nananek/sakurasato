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
- **外部 GET（リモートメディア取得・OGP）は media-proxy のみに許可**。本体 server は信頼できないバイト列をデコードしない。
- **WebFinger 解決も media-proxy 経由**（M10、PR #51 で追加）。`follow <acct>` CLI は media-proxy の `/v1/webfinger/resolve` を通る ── SSRF / redirect / `max_bytes` を画像取得と同じ防御で共有し、`acct:` → actor URI 解決の外向き HTTP が server から消える。
- **外部 POST（ActivityPub 配送 / remote actor fetch）は当面 server から直接行う**（暫定）。Mastodon / Misskey / Pleroma も server 直配送が業界標準で、配送経路を media-proxy に通す利点は限定的なため。これは M6 で media-proxy を実装した時点で再評価する選択（[Issue #23](https://github.com/nananek/sakurasato/issues/23)）。**M6 / M10 完了時点でも本方針を維持**（配送 POST と remote actor fetch のレスポンスは JSON のみで画像デコードを伴わず、媒介化の利得が薄い。一方 WebFinger は鍵を要さない単純 GET なので媒介化済み）。配送経路自体の隔離は将来の独立 issue で再評価。
- **外部 GET（リモートメディア / OGP / WebFinger）は media-proxy のみ**。M6 で TUI のアバター取得経路も `/api/v1/media/proxy` 経由 → media-proxy に切り替え済み（[Issue #36](https://github.com/nananek/sakurasato/issues/36) 解消）── TUI ホストプロセスから直接外向き接続が出なくなり、ホスト LAN / クラウド IMDS への SSRF 表面が縮小。M10 では WebFinger も同 egress 経路に寄せた。
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
- **対応アクティビティ** (M11 完了時点 = inbox dispatch 完全化):
  - **受信実装済み**: `Follow` / `Accept` / `Reject` (M3), `Like` / `EmojiReact` (M8), `Undo` (M8 Reaction + M11 Announce), **`Move`(`movedTo`/`alsoKnownAs`)** (M9), **`Create`/`Note`** (M11 ── followee 投稿 + 我々宛 mention/reply のみ取り込み)、**`Delete`** (M11 ── 既知 Note の author 削除を反映)、**`Update`** (M11 ── Actor は [`crate::remote_actor::fetch_and_upsert`] 再走、Note は content/summary/`edited_at` 上書き)、**`Announce`** (M11 ── followee の boost を既知 Note にのみ記録)。
  - **送出実装済み**: `Create`/`Note` (M3), `Follow` / `Accept` (M3), `Like` / `EmojiReact` / `Undo` (M8), 自身の `Move` 送出 (M9)。
  - **未対応 (低頻度・将来作業)**: `Add` / `Remove` / `Block` / `Flag` / `Question` / `Read` / `View` 等。`crates/server/src/dispatch.rs` の `_ =>` fallback で `202 ACCEPTED` を返した上で debug ログのみ残す (連合相手の再送ループ抑止)。
  - **M11 受信側の意図的スコープ外**: (a) Announce 受信時の未知 Note の自動 fetch ── 他人の boost で見知らぬ note を引き込まないため debug ログのみ。(b) Follow の Undo (= remote 側からのフォロー解除) ── 必要になった時点で `handler::handle_undo_follow` を生やす。
- **Actor 構成**: 単一ユーザー actor ＋ `instance.actor`(application actor, 必要に応じて生成)。
- **ローカル API（server ⇄ tui）**: Unix ドメインソケット上の REST + SSE（タイムライン購読）。お一人様前提でソケットのファイルパーミッションが認証境界。トークン発行は CLI から可能。**メディアアップロード**エンドポイント（アイコン/ヘッダ/添付）を持ち、受領後 media-proxy でサニタイズ・変換 → versitygw 格納 → メタデータを DB 登録。
- **最小 Web UI**: 投稿のパーマリンク（AP Note を人間可読 HTML で）、WebFinger/NodeInfo/actor JSON、メディア配信エンドポイント `GET /media/<key>`（versitygw から取得して配信。versitygw 自体は非公開）。
- **管理 CLI**（同バイナリのサブコマンド, `clap`）: `init`（ユーザー/鍵生成）, `emoji import <zip>`, `follow <acct>`（M10）, `move-accept --from <file>`（M10、inbound `Move` 再処理）, `move-out <target>`（送出側 Move）, `alias add|remove|list|clear`, `token issue|list|revoke`, `deliver --queue-id` など。**Web 認証 UI は作らない。**
  - **`follow <acct>`** は media-proxy で WebFinger を解決し、Follow を `delivery_queue` に積む。`--actor-uri` で WebFinger をスキップして直接 actor URI 指定も可能。`follow-cli-{follower}-{followed}` 形式の決定論的 activity id で `(follower, followed)` UNIQUE 制約と冪等。既存 row が `accepted` なら no-op、`rejected` は明示拒否、`pending` は再 enqueue。
  - **`move-accept` は HTTP 署名検証を通らない**ため、**自分が控えておいた activity 本文** (= 通常経路で受領したものを保存しておいた JSON) でのみ使うこと。第三者から渡された JSON を流すと「Move を勝手に偽装」の入り口になる。コードレベルのガードは「`type == "Move"`」のみで、補完的には `handle_move` 内の `alsoKnownAs` 双方向同意検査・target actor の fresh fetch が「署名なしの任意 Move 適用」を防ぐ。

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
- **隔離**: 本体とは Unix ソケット（または内部ネットのみ）で通信し、外部公開なし。**信頼できないバイト列の取得とデコードを担当するのはこのコンテナだけ**。`mem_limit` を設定し、信頼できない入力のデコードが暴走しても OOM kill で本体に波及させない。なお ActivityPub の配送 POST と remote actor fetch は暫定的に server 直で行う（§3 / [Issue #23](https://github.com/nananek/sakurasato/issues/23)）── 受領レスポンスは JSON のみで画像デコードを伴わないため。
- nekonoverse の `media-proxy-rs`（変換専用・ソケット限定・512M）と `summary-proxy`（SSRF 対策）の役割を Rust の一コンテナに統合。必要なら変換と取得をさらに分割可能な構成にしておく。

#### M6 実装スコープ（現在）
- `POST /v1/image/fetch` — リモート URL を取得 → デコード → variant にリサイズ → WebP 再エンコード。SSRF 検査 + redirect ごとの再検証 + `image::Limits`（画素数上限）+ ストリーミング受信での `max_bytes` 強制。
- `POST /v1/image/sanitize` — 受け取ったバイト列を変換パイプラインに通して再エンコード。M7 アップロードで使う。
- `GET /healthz` — liveness。
- `Variant`: `avatar` (256x256) / `thumbnail` (320x320) / `preview` (1280x1280) / `header` (1500x500)。
- OGP/summary 取得は本 milestone では未実装（呼び出し側が無いため）。M9 もしくは M8 で必要に応じて追加。

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
| `media-proxy` | distroless/static（自作） | 内部のみ・**egress 許可（外部 GET / 画像取得）** | `mem_limit` 設定、本体とソケット通信 |
| `server` | distroless/static（自作） | リバースプロキシ経由で外部 ＋ **egress 許可（AP 配送 POST / remote actor fetch、暫定 #23）** | TUI 用 Unix ソケットをホストへマウント |
| `proxy`(任意) | caddy 等 | 443 | 連合に必須の TLS 終端 |

TUI クライアントはコンテナ外（ホスト端末）で実行し、マウントされた Unix ソケットへ接続。

---

## 7. セキュリティ方針（全コンテナ共通・厳守）

- **distroless ベース**: Rust 静的バイナリ（`x86_64-unknown-linux-musl` 等）→ `gcr.io/distroless/static` か `scratch`。マルチステージビルド。
- **rootless**: 非 root UID で実行（`USER nonroot` / 数値 UID）。
- `read_only: true`（root fs）＋ 必要箇所のみ `tmpfs`(/tmp)。
- `cap_drop: [ALL]`、`security_opt: ["no-new-privileges:true"]`。
- **ネットワーク分離**: versitygw・postgres は内部ネットのみ。**media-proxy は外部 GET（画像 / OGP / WebFinger）の egress を持つ**。**server は AP 配送 POST と remote actor fetch のみ外部に egress を持つ**（暫定、§3 / [Issue #23](https://github.com/nananek/sakurasato/issues/23)）── M10 で再評価し配送経路は当面 server 直のまま維持、WebFinger は M10 PR #51 で media-proxy 経由に移行。
- **危険な入力の隔離**: 画像デコード・外部 GET は必ず media-proxy 側。server は信頼できないバイト列を直接デコードしない（AP 配送と actor fetch のレスポンスは JSON のみで扱う）。

### 7.1 シークレット管理

- **HTTP 署名鍵 (RSA + Ed25519)** は `init` CLI で生成し、`actor.private_key_pem` / `actor.ed25519_private_key_pem` に **DB 保管**。
  - `ActorRow` で `#[serde(skip)]` + `Debug` redacted（[`crates/core/src/model.rs`](crates/core/src/model.rs)）── ローカル API / `tracing::debug!(?actor)` 経由で漏れない。
  - マスアサインメント脆弱性（外部 JSON → `serde_json::from_value::<ActorRow>` で書き戻し）も `#[serde(skip)]` で塞ぐ。
  - バックアップは postgres の物理バックアップに乗る（鍵単独のファイル管理は不要）。
  - 鍵ローテーション = `init --force`。**フェデレーション破壊** (= 既存フォロワーの inbox 配送が署名検証失敗で全部弾かれる) なので緊急時のみ。
- **compose secrets** (`secrets/postgres_password.txt`, `secrets/s3_secret_key.txt`)
  - **`secrets/` ディレクトリは `chmod 700`** ── ホスト上の他ユーザから secrets を見せない一次防御。ディレクトリ traversal は docker daemon (= ホスト `$USER`) が行うので、コンテナ側の通信には影響しない。
  - **ファイル自体は `chmod 644`** で OK (rootless Docker 環境では `chmod 600` だと **postgres コンテナ (uid 70) が読めず起動失敗** する。検証済み: ホスト `$USER` 所有 0600 ファイルは container uid 0 (= subuid マップで host `$USER`) のみ読み取り可、container uid 70 (= host subuid 100070) は権限なし)。
  - **compose v5 (bind-mount secret) は `uid` / `gid` / `mode` を上書きできない** (compose が `WARN[0000] secrets.postgres_password: 'mode' is not supported by compose` を出す既知の制約) ため、ホストファイルの mode がそのままコンテナ内に出る。
  - **本番 (rootful Docker / Docker Swarm)** では `chmod 600 secrets/*.txt` + ファイル所有者を実際の postgres / versitygw コンテナ uid に合わせる、もしくは `swarm secret` (compose v3 swarm mode で `external: true`) を使うのが王道。本リポジトリは dev / single-host を主想定とするため bind-mount + 0700 ディレクトリで運用する。
  - **`secrets/.gitignore`** が `*` で全 ignore、`!example.txt` と `!.gitignore` のみ追跡 ── 実シークレットは絶対にコミットされない。
  - ローテーション: `docker compose down` → `secrets/*.txt` を新値で書き換え → `docker compose up`。サービス断は postgres / versitygw が起動するまでの数秒。
- **ローカル API トークン** (M4): `api_token` テーブルに **ハッシュのみ** 保管 (`token issue` 時に raw 値を一度だけ stdout に出す)。再表示不可なので失敗時は `revoke` → 再 `issue`。

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

# シークレットを準備 (§7.1)
# - ディレクトリは 0700 でホスト上の他ユーザから遮断
# - ファイルは rootless Docker でも postgres uid 70 が読めるよう 0644
# (※ 0600 は rootless 環境で postgres コンテナが起動失敗するので避ける。
#    本番 rootful Docker では 0600 + 適切な chown を §7.1 参照)
chmod 700 secrets
# 既存 secrets/postgres_password.txt / s3_secret_key.txt が既に存在する場合は
# 上書きしないこと (= 既存 DB / バケットが復号できなくなる)。初回のみ:
# `-hex 32` (= 256 bit) で `=` パディングと改行混入を避ける ── postgres /
# S3 接続文字列に貼ったときに URL エンコードでハマらないよう。
[ -f secrets/postgres_password.txt ] || openssl rand -hex 32 > secrets/postgres_password.txt
[ -f secrets/s3_secret_key.txt ]    || openssl rand -hex 32 > secrets/s3_secret_key.txt
chmod 644 secrets/postgres_password.txt secrets/s3_secret_key.txt
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

# 起動（開発, ローカル build）
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build
# server 管理 CLI
cargo run -p sakurasato-server -- init
# TUI（ホスト端末）
cargo run -p sakurasato-tui

# 起動（本番風, ghcr 発行済みイメージを pull）
# ─ §11 publish.yml で発行された ghcr.io/nananek/sakurasato-* を使う
# ─ :latest = 最新 CalVer リリースに追従。固定したい場合は overlay の
#   `image:` を `:YYYY.MM.patch` に書き換える
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml pull
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml up -d
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
- **タグ push で `.github/workflows/publish.yml` が発火**し、`ghcr.io/nananek/sakurasato-{server,media-proxy,versitygw}` に `:YYYY.MM.patch` + `:latest` を public で push する (linux/amd64)。`develop` への push は `:develop` のみ。手動再発行は GitHub UI の `workflow_dispatch` から。

### コミット規約
- **Conventional Commits**。type 例: `feat` / `fix` / `docs` / `refactor` / `test` / `perf` / `chore` / `ci` / `docker` / `deps`。
- 1 行目: `type(scope): 要約`。本文に理由。Claude 作業分は末尾に `Co-Authored-By: Claude ...`。

### git hooks（`.githooks/`, `core.hooksPath` で有効化）
- **`pre-push`**: `main` への直接 push を拒否（**タグ push と develop は許可**）。
- **`pre-commit`**: `main` ブランチ上での直接コミットを拒否。
- 有効化（クローン後に一度）: `git config core.hooksPath .githooks`。リモートの branch protection と二重で守る。

### CI / 自動化（`.github/`）
- **CI** (`ci.yml`): `cargo fmt --check` / `clippy -D warnings` / `test`。`main`・`develop` の push と PR。required status check = `ci`。`Cargo.toml` が無い間はスキップして緑（M1 で本稼働）。
- **Publish** (`publish.yml`): タグ push (`YYYY.MM.patch`) と `develop` push で発火。ghcr に `sakurasato-{server,media-proxy,versitygw}` を public で push する。BuildKit + GitHub Actions cache (`type=gha,scope=<name>`) を使うことで 3 イメージ並列ビルドが現実時間内に収まる。**新しい deploy host は `docker compose -f docker-compose.yml -f docker-compose.ghcr.yml pull` で取得**（§9 参照）。
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
- **`sakurasato-server follow <acct>`** (M10) で実 Misskey 等への WebFinger 解決 → Follow 送出 → 相手側 Accept が返って `follow.state = accepted` になることを確認。`--actor-uri` 直指定経路と WebFinger 経路の双方をカバーする。
- **`sakurasato-server move-accept --from <file>`** (M10) で保存しておいた inbound `Move` 本文を再処理し、`alsoKnownAs` 検査と自動 re-follow が通常経路と同じく走ることを確認。
- egress 制限の確認: server コンテナから外部 URL 取得 (画像 / WebFinger) が直接できず、media-proxy 経由のみで成立すること。`docker exec sakurasato-server-1 wget -O- https://example.com` 等で接続が失敗することを確認。
- **連合自動テスト (M12 / Issue #56)** ── `./scripts/federation-test/pytest.sh mastodon` で pytest stack を立ち上げ、Sakurasato ↔ Mastodon の WebFinger / Actor / Follow / Note / Reaction 連合を programmatic に駆動する。Sakurasato 側は `/api/v1/*` (UDS + Bearer) を、Mastodon 側は entrypoint で発行した Doorkeeper access token を使う。`.github/workflows/federation-test.yml` が nightly cron + `workflow_dispatch` で同じ stack を回す (= required check ではない、外部 image の更新で揺らぐため)。後続 PR で Misskey / Pleroma / Mitra / Fedibird / Nekonoverse を同じパターンで追加していく予定。
