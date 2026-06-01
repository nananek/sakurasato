# Sakurasato デプロイガイド 🌸

> このドキュメントは Sakurasato を **公開 Fediverse サーバ** として本番運用するための手順をまとめています。お一人様サーバなのでミスもバックアップも全部自分の責任です。慎重に進めてください。

## 0. 想定構成

```
┌───────────────────────── 外部 ─────────────────────────┐
│  Fediverse (Misskey / Mastodon / ...) ──HTTPS 443──┐  │
│  Cloudflare edge ─────cloudflared tunnel───────────│  │
└────────────────────────────────────────────────────│──┘
                                                     │
┌─────────────────────── ホスト機 ───────────────────│──┐
│  cloudflared (container or systemd)                │  │
│        ↓ http://server:8080                        │  │
│  ┌──── docker compose (rootless or rootful) ──────│┐ │
│  │  server / media-proxy / postgres / versitygw   ││ │
│  └────────────────────────────────────────────────┘│ │
│  Tailscale tailnet (TUI 経路専用)                   │ │
└─────────────────────────────────────────────────────│─┘
                       │
                  別端末 (TUI クライアント)
```

- **公開**: Cloudflare Tunnel が AP 連合の HTTPS 443 を担い、TLS 終端と自宅 IP 隠蔽を Cloudflare 側で行う。ホスト機は外部に直接ポートを開けない。
- **TUI 経路**: Tailscale tailnet 内のみ。TUI クライアントは別端末から tailnet IP 経由でホストに到達し、`/run/sakurasato-local/local.sock` (Unix socket) を経由してローカル API を叩く。
- **caddy / nginx は使わない**: TLS は Cloudflare 任せにして、自前のリバースプロキシは持たない。

詳細な設計根拠は [CLAUDE.md §3](CLAUDE.md#3-アーキテクチャ全体像) と [本リポジトリの memory ([[deployment-tailscale-cloudflared]])](https://github.com/nananek/sakurasato/issues/11) を参照。

---

## 1. 前提条件

- **ホスト機**: 常時稼働する Linux 機（自宅サーバ / VPS / Raspberry Pi 等）
  - Docker 25+ または rootless Docker 推奨。`docker compose` v5+
  - 最低 2 GB RAM（media-proxy `mem_limit: 512m` + postgres + server + versitygw）
  - 数 GB の空きディスク（postgres data + versitygw blob + media）
- **公開ドメイン**: 自分の所有するドメインを Cloudflare DNS に乗せておく
  - 例: `sakurasato.example.com` を Sakurasato 用に割り当てる
- **Cloudflare アカウント**: Cloudflare Tunnel (`cloudflared`) を使うのに必要（無料プランで可）
- **Tailscale アカウント**: TUI 経路用（無料プランで可）

---

## 2. ホスト機の準備

### Docker / docker compose

[公式手順](https://docs.docker.com/engine/install/) または各ディストリの rootless Docker パッケージで入れる。本リポジトリは rootless Docker での動作を検証している ([[user-arch-rootless]] / CLAUDE.md §7.1)。

```bash
# rootful の場合
sudo systemctl enable --now docker

# rootless の場合 (Arch Linux 例)
sudo pacman -S docker docker-compose
systemctl --user enable --now docker
export DOCKER_HOST="unix:///run/user/$(id -u)/docker.sock"
```

### Tailscale

```bash
# Arch Linux 例
sudo pacman -S tailscale
sudo systemctl enable --now tailscaled
sudo tailscale up
# tailnet に参加 → 他端末からも `tailscale up` で参加させる
```

### cloudflared

[公式ドキュメント](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/) を参照。tunnel を作成して認証情報 (`*.json`) を取得しておく。

```bash
cloudflared tunnel login
cloudflared tunnel create sakurasato
# 生成された tunnel UUID と credentials JSON を控える
```

---

## 3. Sakurasato のセットアップ

### 3.1 リポジトリ取得

```bash
git clone https://github.com/nananek/sakurasato.git
cd sakurasato
```

### 3.2 シークレット生成

```bash
# CLAUDE.md §7.1 のセキュリティ方針:
#  - secrets/ ディレクトリ = chmod 700 (ホスト他ユーザから遮断)
#  - 各ファイル              = chmod 644 (rootless Docker での postgres uid 70 が読めるよう)
# 本番が rootful Docker / Swarm なら 0600 + 適切な chown も可
chmod 700 secrets
[ -f secrets/postgres_password.txt ] || openssl rand -hex 32 > secrets/postgres_password.txt
[ -f secrets/s3_secret_key.txt ]    || openssl rand -hex 32 > secrets/s3_secret_key.txt
chmod 644 secrets/postgres_password.txt secrets/s3_secret_key.txt
```

`-hex 32` (= 256 bit) を使うのは、`-base64 32` だと `=` パディングが付いて接続文字列の URL エンコードでハマるケースがあるため。

### 3.3 設定ファイル

設定は 3 段階で merge される (実装は [`crates/core/src/config.rs`](crates/core/src/config.rs)):

1. **base TOML** (`SAKURASATO_CONFIG` env、既定 `config/default.toml` ── 既にイメージに焼き込み済み)
2. **overlay TOML** (CLI `--config <path>`、optional)
3. **環境変数 `SAKURASATO_*`** (最終上書き、本番ではこれを使うのが楽)

ghcr のコンテナイメージには `config/default.toml` が焼き込まれているので、**本番では環境変数で必要な項目だけを上書きする**のが推奨。`docker-compose.yml` の `server.environment` セクションに項目を足す形になる。

最低限の追加例:

```yaml
# docker-compose.override.yml (例)
services:
  server:
    environment:
      SAKURASATO_SERVER__HOST: "sakurasato.example.com"
      SAKURASATO_SERVER__USER: "me"
      # bind / local_api_socket は既存のままで OK (8080 + UDS)
```

`__` (アンダースコア 2 つ) がネスト区切り (`server.host` → `SAKURASATO_SERVER__HOST`)。詳細な変数名は `crates/core/src/config.rs` の `ServerConfig` / `StorageConfig` / `DatabaseConfig` / `MediaProxyConfig` を参照。

別案として、host 側 TOML を bind mount + `--config` で渡す方法もある:

```yaml
services:
  server:
    volumes:
      - ./config/local.toml:/app/config/local.toml:ro
    command: ["serve", "--config", "/app/config/local.toml"]
```

### 3.4 イメージ取得 (ghcr)

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml pull
```

ghcr では以下の 3 イメージが発行されている:

- `ghcr.io/nananek/sakurasato-server`
- `ghcr.io/nananek/sakurasato-media-proxy`
- `ghcr.io/nananek/sakurasato-versitygw`

タグ:
- `:latest` — 最新 CalVer リリースに追従（**本番推奨**: 差し戻し可能 + 監査ログ残る）
- `:YYYY.MM.patch` — 特定バージョンに固定したい場合
- `:develop` — develop HEAD（**本番非推奨**、動作確認用）

特定バージョンに固定するには `docker-compose.ghcr.yml` の `image:` を `:2026.05.0` 等に書き換える。

### 3.5 初期化

```bash
# DB マイグレーション + ローカル actor + RSA / Ed25519 鍵生成 (init)
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server init
```

`init` は以下を実行する:
1. `sqlx::migrate!` で DB スキーマを最新化
2. 設定の `server.user` + `server.host` で local actor を作成
3. RSA 2048 + Ed25519 鍵ペアを生成し `actor.private_key_pem` / `actor.ed25519_private_key_pem` に DB 保存（`#[serde(skip)]` + Debug redacted で漏洩防止）

再 keying は `init --force`。**フェデレーション関係が事実上ゼロからやり直しになる不可逆操作** ── 既存フォロワーへの配送は全部署名検証失敗で弾かれ、ロールバック手段は postgres dump 復元のみ。緊急時 (鍵漏洩等) のみ。詳細と挙動は §7.3 鍵ローテーションを参照。

### 3.6 起動

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml up -d
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml ps
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs -f server
```

### 3.7 (任意) 外部 managed 構成 ── Neon + Cloudflare R2 等

§3.1〜§3.6 の本流は postgres + versitygw を **ローカル compose 内で立てる前提**。リソース制約のあるホスト (例: 962MB RAM クラスの VPS) ではコンテナを 2 つ削って外部 managed サービスへ逃がしたい場合がある。

設定面では `crates/core/src/config.rs` の `DatabaseConfig` / `StorageConfig` が env による完全上書きを許しているので、**コンテナ起動レイヤで postgres / versitygw を外す + server に managed サービスの接続情報を env 注入する** だけで切替できる。

実証: 2026-05-31 / 962MB VPS 上で Neon (managed Postgres) + Cloudflare R2 (S3 互換) の組合せで連合が成立、アイドル時 RAM は server + media-proxy + cloudflared で計 45 MiB 程度。

#### overlay 例

```yaml
# docker-compose.managed.yml ── §3.3 で書いた docker-compose.override.yml と
# 同居させ、compose の左→右で順に重ねる:
#   docker compose -f docker-compose.yml \
#                  -f docker-compose.ghcr.yml \
#                  -f docker-compose.managed.yml up -d

services:
  # `disabled` profile は通常起動では空集合に該当しないので、これらは
  # `compose up` の対象から外れる (= イメージ pull もコンテナ作成も走らない)。
  postgres:
    profiles: ["disabled"]
  versitygw:
    profiles: ["disabled"]

  server:
    # base compose の depends_on を打ち消し、media-proxy だけに依存させる
    # (postgres / versitygw は外部 managed なので docker が待つ意味がない)。
    # `!override` は Compose CLI v2.13+ で利用可能。
    depends_on: !override
      media-proxy:
        condition: service_started
    environment:
      # ─── Neon ─────────────────────────────────────────────
      # `{password}` placeholder は SAKURASATO_DATABASE__PASSWORD_FILE の
      # 内容で実行時置換される (RFC 3986 percent-encoded)。
      SAKURASATO_DATABASE__URL: "postgres://nekonon:{password}@ep-xxxx.region.aws.neon.tech/sakurasato?sslmode=require"
      SAKURASATO_DATABASE__PASSWORD_FILE: "/run/secrets/neon_password"
      # ─── Cloudflare R2 ───────────────────────────────────
      SAKURASATO_STORAGE__ENDPOINT: "https://<account-id>.r2.cloudflarestorage.com"
      SAKURASATO_STORAGE__BUCKET: "sakurasato-media"
      SAKURASATO_STORAGE__REGION: "auto"
      SAKURASATO_STORAGE__ACCESS_KEY_ID: "<R2 API token access key>"
      SAKURASATO_STORAGE__SECRET_ACCESS_KEY_FILE: "/run/secrets/r2_secret_access_key"
    secrets:
      - neon_password
      - r2_secret_access_key

secrets:
  neon_password:
    file: ./secrets/neon_password.txt
  r2_secret_access_key:
    file: ./secrets/r2_secret_access_key.txt
```

secrets ファイルの権限は §3.2 と同じ運用 (`secrets/` = `chmod 700`、各ファイル = `chmod 644`)。base compose 側で参照されている `postgres_password` / `s3_secret_key` は server 側で env を上書きしているので未使用になるが、サービス定義に残っていてもエラーにはならない (= base 側を編集する必要はない)。

#### Neon 注意点

- **`?sslmode=require` 必須**。Neon は平文接続を受け付けない。
- Neon のダッシュボードから払い出される接続文字列には `channel_binding=require` が付いていることがある。sqlx 0.9 はこのパラメタを認識せず起動毎に `unknown URL parameter: channel_binding` の WARN を吐くため、`DatabaseConfig::resolved_url` 側で**実行時に剥がす** (Issue #74)。channel binding は Neon 側で TLS 終端時に強制されており、クライアント側の advisory フラグは外しても接続安全性に影響しない (結局 `SCRAM-SHA-256-PLUS` を要求される)。
- `init` で `sqlx::migrate!` が走るため、`docker compose run --rm server init` をそのまま実行すれば Neon DB にマイグレーションが適用される。

#### Cloudflare R2 注意点

- **バケットは事前に R2 ダッシュボードで作成しておくこと**。sakurasato は `CreateBucket` を呼ばないので、存在しないとメディアアップロードで 404 になる。
- **region は文字列 `"auto"`** を渡す。R2 は単一 region 扱い。
- **`force_path_style(true)`** ([`crates/server/src/state.rs`](crates/server/src/state.rs) `build_s3_client`) は R2 でも問題ない。R2 は path-style / virtual-hosted-style の両方を受ける。
- R2 API token は読み取り + 書き込み権限のある **`Object Read & Write`** スコープで発行する。

#### init 後の動作確認

`init` は actor の生成と DB マイグレーションだけで、**storage に対しては一切 I/O を出さない**。R2 credentials の入力ミス (例: `access_key_id` のタイポ / 誤ったバケット名) は init では検知できず、最初のメディアアップロード (= TUI でのアイコン変更や添付投稿) まで顕在化しない。`up -d` 後は以下を踏むのが安全:

```bash
# 1. server の起動ログを見て、DB 接続失敗 / `endpoint` resolve 失敗が無いことを確認
docker compose ... logs -f server

# 2. TUI でアイコンを差し替えるか、画像添付の投稿を 1 回試して 200 を確認
#    (失敗ログは `tracing` レベル WARN/ERROR で出る)
```

DB と storage の片方ずつしか壊れていないと server は起動できてしまうので、メディアアップロードまで通って初めて managed 構成全体が成立したと言える。

---

## 4. Cloudflare Tunnel の設定

ホスト機の `server` コンテナを Cloudflare Tunnel に渡す。cloudflared を **同じ compose の追加サービス** として動かすのが楽。

cloudflared の **Service** には 2 経路ある:

- **TCP** (= 既定): `http://server:8080`。compose 内部ネットの DNS で解決される。
- **UDS** (#69, 推奨): `unix:/run/sakurasato/public.sock`。共有 volume 越しに繋ぐ。host にポートが出ない (= scan / 直接アクセスの表面が消える)。

UDS 経路にする場合は `server` 側で `SAKURASATO_SERVER__PUBLIC_LISTEN=unix:/run/sakurasato/public.sock` を入れ、cloudflared サービスにも同じ volume をマウントする。cloudflared ダッシュボードで Service を `unix:/run/sakurasato/public.sock` に登録すれば動く。

### 4.1 token を env_file で注入

CLI 引数 (`--token "${TOKEN}"`) に渡すと `ps` / `docker inspect` で平文露出するので避ける。

**`cloudflare/cloudflared:latest` は distroless** (`/bin/sh` を持たない) のため、従来の `entrypoint: ['/bin/sh', '-c', 'exec cloudflared --token "$(cat /run/secrets/...)" run']` パターンは「`OCI runtime exec failed: exec: "/bin/sh": stat ...`」で起動失敗する ([Issue #71](https://github.com/nananek/sakurasato/issues/71))。

代わりに **`TUNNEL_TOKEN` 環境変数** (= cloudflared の公式入力経路) を `env_file` で注入する:

```bash
# ホスト側 ── KEY=VALUE 形式の単一行ファイル。
printf 'TUNNEL_TOKEN=%s\n' "<YOUR_CLOUDFLARED_TOKEN>" > secrets/cloudflared.env
chmod 644 secrets/cloudflared.env   # secrets/ ディレクトリは 0700 (§7.1)
```

`env_file` 経由ならコンテナ内に環境変数として直接展開され、`ps` / `docker inspect` (= 引数を引く) では値が見えない。compose は `env_file` をコンテナ起動時に解釈するだけで、host process 環境にも乗らない。

### 4.2 cloudflared overlay

```yaml
# docker-compose.cloudflared.yml (例)
services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    restart: unless-stopped
    # 公式イメージの ENTRYPOINT は `cloudflared` 単体。`TUNNEL_TOKEN` env で
    # token を渡せば `tunnel run` のサブコマンドだけ command として足せば良い
    # (= distroless なので /bin/sh ラッパは使えない、Issue #71)。
    command:
      - tunnel
      - --no-autoupdate
      - run
    env_file:
      - ./secrets/cloudflared.env
    networks:
      # server は internal + egress 両方に居る (CLAUDE.md §6)。
      # cloudflared は外部 (= Cloudflare edge) と server だけ届けばよいので
      # `egress` 一本でよい (server も egress を持つので名前解決で疎通する)。
      - egress
    depends_on:
      - server
```

`compose -f docker-compose.yml -f docker-compose.ghcr.yml -f docker-compose.cloudflared.yml up -d` で 4 サービス + tunnel が立つ。

cloudflared ダッシュボードで:
- **Public Hostname** → `sakurasato.example.com`
- **Service** → `http://server:8080`

を登録する。タイムアウト・HTTP/2 設定は標準で問題なし。

### 4.3 動作確認

```bash
curl -sv "https://sakurasato.example.com/.well-known/webfinger?resource=acct:me@sakurasato.example.com" | head
curl -sv "https://sakurasato.example.com/users/me" | head
```

actor JSON が返れば連合準備完了。

---

## 5. TUI 経路 (Tailscale)

別端末から TUI を叩くには **server のローカル API listener を TCP に切替えて Tailscale tailnet に直出し** するのが推奨 (#69 で実装、2026-05 以降)。
従来の **socat ブリッジ + tailscale serve** 構成 (= 後述 5.2 / 5.3) も互換性のため残すが、新規構築では `local_api_listen` で TCP に倒す方が一手 (= bridge コンテナ + tailscale serve 不要)。

### 5.1 推奨: `local_api_listen` を TCP に倒す (#69)

```yaml
# docker-compose.override.yml (例)
services:
  server:
    environment:
      # 既定の UDS 経路を捨て、Tailscale 越しに別端末の TUI が直接叩ける TCP に倒す。
      # **container 内 0.0.0.0**: docker bridge 越しに `ports:` で host に
      # 出すための bind (= container 内 LAN しか開かない、外には出ない)。
      SAKURASATO_SERVER__LOCAL_API_LISTEN: "tcp://0.0.0.0:18080"
    ports:
      # **重要**: 必ず `127.0.0.1:` を前置する。`"18080:18080"` だと docker が
      # `0.0.0.0:18080:18080` 扱いで一般 LAN にも漏れる。
      # 127.0.0.1 に限定すれば host loopback だけに上がり、Tailscale が tailnet
      # に流す相手はこの loopback ポート (host) になる。
      - "127.0.0.1:18080:18080"
```

公開経路の選択肢は 2 通り。**いずれも一般 LAN へは出ない構成**にすること:

1. **`tailscale serve` で tailnet HTTPS** ── host で
   `sudo tailscale serve --tcp 18080 tcp://127.0.0.1:18080` を立て、tailnet
   ノードから `https://<host>.<tailnet>.ts.net/` で受ける。TLS 終端は Tailscale。
2. **Tailscale ACL + 直接 TCP** ── host の Tailscale ACL で port 18080 への
   tailnet 内アクセスだけを許可し、`http://<host>.<tailnet>.ts.net:18080` で
   接続する。WireGuard が暗号化するため平文 HTTP でも盗聴されない (TUI 側で
   `--api-url https://` は **未対応**: `crates/tui/src/main.rs::resolve_endpoint`
   が明示拒否する。理由は TUI に TLS スタックを抱えない設計 ── [PR #70 review])。
   ACL 設定方針の参考: <https://tailscale.com/kb/1018/acls>

TUI 側 (= ノートパソコン等):

```bash
# Tailscale tailnet 内のホスト名 (MagicDNS) で接続
sakurasato-tui --api-url http://<host>.<tailnet>.ts.net:18080 \
  --token-file ~/.config/sakurasato/tui.token
```

`Authorization: Bearer <token>` ヘッダはそのまま流れ、認証は **Bearer + tailnet ACL** の二重壁。UDS の `chmod 0o600` は使えなくなるが、tailnet 自体が closed network なので安全に差し替えられる。

### 5.2 旧法 (socat ブリッジ + tailscale serve)

UDS 運用を維持したい場合は従来どおりブリッジを噛ます。`docker-compose.yml` の `server` サービスは `/run/sakurasato-local/local.sock` を `local_sock` named volume にマウントしている。本体は HTTP (REST + SSE) を喋るが Unix socket 上なので、別端末から叩くには **UDS↔TCP ブリッジコンテナを挟む + tailscale serve で tailnet に HTTPS で出す** 構成になる。

> Tailscale 自身は UDS をそのまま serve できない (`tailscale serve --tcp <PORT>` は TCP backend 必須)。`tailscale serve --bg /path/to/socket` 系の用法も無いので、必ずブリッジを 1 個挟む。

#### 5.2.1 socat による UDS→TCP ブリッジ

```yaml
# docker-compose.tui.yml (例)
services:
  # UDS (local_sock 内 /run/sakurasato-local/local.sock) を loopback TCP 8443 に
  # 中継する。本体 server の Bearer 認証はそのまま通る (= HTTP ヘッダのまま転送)。
  uds-tcp-bridge:
    image: alpine/socat:latest
    command: TCP-LISTEN:8443,fork,reuseaddr UNIX-CONNECT:/run/sakurasato-local/local.sock
    volumes:
      - local_sock:/run/sakurasato-local
    networks:
      - internal
    # host loopback にだけ晒す。tailscale serve がここを upstream に取る。
    # 0.0.0.0 公開は厳禁 (= tailnet 経由のはずが外向き露出する)。
    ports:
      - "127.0.0.1:8443:8443"
    restart: unless-stopped
```

#### 5.2.2 tailscale serve で tailnet に出す

```bash
# ホスト機
tailscale serve --bg --https 443 http://127.0.0.1:8443
# 公開状態を確認
tailscale serve status
```

これで `https://<host-machine>.<tailnet>.ts.net/` (= MagicDNS が振った名前) で **tailnet 内のノードからだけ** 本体ローカル API に到達できる。`tailscale serve` は外部からは到達不可。`tailscale funnel` (= 公衆公開) は **絶対に使わない** ── ローカル API が外に出る。

#### 5.2.3 トークン発行

ブリッジ越しでも Bearer 認証は維持される (HTTP ヘッダ素通し)。

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server token issue --name tui-laptop
# 出力された raw token を TUI 側に控える (この一度しか表示されない)
```

TUI 側 (= 別端末、ノートパソコン等) は `https://<host>.<tailnet>.ts.net/` + Bearer token を設定して接続する。

#### 5.2.4 代替: SSH で host に入って TUI を local 実行

ブリッジが面倒なら、tailnet 越しに SSH してホストで TUI を直接動かす手もある (= UDS をコンテナ外に晒さないでよい):

```bash
ssh <host>.<tailnet>.ts.net   # tailscale ssh でも可
# host 機上で
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm \
  -v /tmp/sks-tui:/tmp/sks-tui \
  server sh -c 'cp /run/sakurasato-local/local.sock /tmp/sks-tui/'
# or もっと素直に: TUI バイナリを host にインストールして直接 UDS を叩く
sakurasato-tui --socket /var/run/sakurasato-local/local.sock
```

ただし host 側で TUI を動かすには **`local_sock` named volume を host bind に切り替える** か、TUI コンテナを compose 内に追加する必要があり、結局構成が増える。socat ブリッジの方が単純。

---

## 6. 連合の動作確認

```bash
# WebFinger
curl -s "https://sakurasato.example.com/.well-known/webfinger?resource=acct:me@sakurasato.example.com" | jq .

# 別の Fediverse サーバから follow
# (Mastodon の検索バーに `@me@sakurasato.example.com` を入れて follow)

# follow が届いたかをログで確認
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs server | grep -i follow

# 自分から相手を follow する (CLI)
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server follow @alice@mastodon.example
```

`follow-cli-{follower_id}-{followed_id}` 形式の activity id が生成され、`delivery_queue` に積まれて常駐ワーカが送出する。

### 6.1 受信できる activity の範囲 (M11 完了時点 = inbox dispatch 完全化)

| activity | 受信 | 備考 |
|---|---|---|
| `Follow` / `Accept` / `Reject` | ✅ | M3 |
| `Like` / `EmojiReact` | ✅ | M8 |
| `Undo` | ✅ | M8 Reaction の取り消し + M11 Announce の取り消し |
| `Move` | ✅ | M9 (`alsoKnownAs` 双方向同意検査あり) |
| `Create` / `Note` | ✅ | M11 ── **followee の投稿** + **我々宛 mention / reply** のみ DB へ insert (= public TL に流れる無関係 post は引き込まない / DB 肥大対策) |
| `Delete` | ✅ | M11 ── 既知 Note のみ削除。`note.actor_id == signer.id` を検査して**第三者削除を拒否**。未知 Note は silent no-op |
| `Update` (Actor) | ✅ | M11 ── `object.id == signer.ap_id` を検査した上で [`remote_actor::fetch_and_upsert`] を再走。公開鍵ローテーション含む |
| `Update` (Note) | ✅ | M11 ── 既知 Note の **作者本人** が編集している場合のみ content / summary / `edited_at` を上書き |
| `Announce` | ✅ | M11 ── **followee** の boost を **既知 Note** にのみ記録。未知 Note は debug ログのみで no-op (= fetch しない、boost で見知らぬ note を引き込まない設計) |

低頻度 activity (`Add` / `Remove` / `Block` / `Flag` / `Question` / `Read` / `View` 等) は `202 ACCEPTED` を返した上で debug ログのみ。Follow の Undo (= remote 側からのフォロー解除) は本リリースでは未対応。

実装状況の追跡は [#11](https://github.com/nananek/sakurasato/issues/11) (全体トラッカ) 参照。

---

## 7. 運用タスク

### 7.1 イメージ更新

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml pull
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml up -d
```

`:latest` を追っている限り、毎回最新リリースに上がる。差し戻したい場合は `docker-compose.ghcr.yml` の `image:` を一つ前のバージョンタグに書き換える。

### 7.2 バックアップ

最低限バックアップすべきもの:

| 対象 | 取り方 | 頻度 |
| --- | --- | --- |
| **postgres** | `pg_dump` (HTTP 署名鍵が DB に乗っているので最重要) | 日次 |
| **versitygw_data** | `docker compose cp` または volume の直接コピー | 日次〜週次 |
| **secrets/** | リポジトリ外で安全に控える (`postgres_password.txt` / `s3_secret_key.txt`) | 変更時 |
| **config/local.toml** | リポジトリ外で控える | 変更時 |

例 (postgres):

```bash
# `-T` (--no-TTY) を必ず付けること ── 付けないと exec が PTY を割り当て、
# gzip 出力に \r\n 変換 / ESC[ シーケンスが混入して **復元不能** な dump になる。
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml exec -T postgres \
  pg_dump -U sakurasato sakurasato | gzip > backup-$(date +%F).sql.gz
```

復元時は対称に `psql` で読む:

```bash
gunzip -c backup-YYYY-MM-DD.sql.gz | docker compose -f docker-compose.yml -f docker-compose.ghcr.yml exec -T postgres \
  psql -U sakurasato sakurasato
```

### 7.3 鍵ローテーション

**通常は触らない**。万一秘密鍵が漏れた場合のみ:

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server init --force
```

実行すると新しい RSA / Ed25519 鍵ペアが生成され、`public_key_id` も変わる。**既存フォロワーは旧鍵 ID をキャッシュしているので、相手側で actor JSON が refresh されるまで配送が全部 401 で弾かれる**。再フォローを依頼する覚悟が必要。

**不可逆性**: 旧鍵は失われ、postgres バックアップから復元する以外に戻せない。`init --force` は §3.5 (初期化) でも触れたが、**ロールバック手段が postgres dump 復元のみ** であることを承知の上で実行すること。フェデレーション関係は事実上ゼロからやり直し。

### 7.4 引っ越し (Move)

別サーバから / 別サーバへの引っ越し手順:

```bash
# 別サーバから本サーバへの引っ越しを受け入れる準備:
# 1. 先方 actor の alsoKnownAs に本サーバの ap_id を登録してもらう
# 2. こちら側の alsoKnownAs にも先方 actor URI を登録する
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server alias add https://old.example/users/me

# 本サーバから別サーバへ引っ越す:
# 1. 移動先 actor の alsoKnownAs に本サーバの ap_id が **先に** 登録されていることを確認
# 2. Move 送出
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server move-out https://new.example/users/me
```

失敗時 (一時的な DB / network エラーで inbound Move が 503 になったケース) は保存しておいた activity 本文で再処理:

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm -v $PWD:/work server move-accept --from /work/move.json
```

**`move-accept` は HTTP 署名検証を通らない**ので、自分が控えておいた本文でのみ実行すること。コードレベルのガードは `type == "Move"` チェックだけで (CLAUDE.md §5.1)、第三者から「この JSON を `move-accept` に渡せばフォロワーを引き継げます」と誘導されて流すと **意図しない Move を適用してしまう**。最後の防波堤は `handle_move` 内の `alsoKnownAs` 双方向検査だが、相手側 actor が攻撃者の意図通りに `alsoKnownAs` を書き換えていれば素通る。ソーシャルエンジニアリングへの耐性は低いので、入力経路を自分の inbox 控えに限ること。

### 7.5 ログ確認

```bash
# 全サービス
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs --tail 200 -f

# 配送ワーカに絞る
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs server | grep -i deliver

# media-proxy のエラー
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs media-proxy | grep -E "warn|error"
```

ログレベルは環境変数 `RUST_LOG` で制御 (例: `RUST_LOG=sakurasato=debug,info`)。本番では `info` 既定で OK。

---

## 8. トラブルシューティング

### 8.1 postgres が起動しない (Permission denied)

```
/usr/local/bin/docker-entrypoint.sh: line 21: /run/secrets/postgres_password: Permission denied
```

→ rootless Docker で `secrets/postgres_password.txt` が 0600 になっている。`chmod 644 secrets/*.txt` に直す。詳細は CLAUDE.md §7.1。

### 8.2 配送が全部失敗する (HTTP 401 from peers)

→ 鍵が peer 側でキャッシュされた古い値と不一致。`init --force` 直後はよくある。

1. **先に peer 側で actor JSON の cache を refresh してもらう** ── 順番が逆だと再送しても 401 が続いて状態が改善しない。Mastodon なら admin、Misskey なら remote actor の再取得 UI から。
2. peer のキャッシュが更新されたのを確認してから `delivery_queue` の failed 行を `pending` に戻す。**全 failed を無差別にリセットするな** ── DNS 失敗・TLS エラー等の他の永続失敗まで巻き込んで無限リトライ砲台になる。特定 inbox URL に絞る:

```sql
-- 例: 特定 peer の inbox のみ再送 (置き換え)
UPDATE delivery_queue
SET state='pending', retries=0, next_attempt_at=now()
WHERE state='failed' AND inbox_url LIKE 'https://mastodon.example/%';
```

### 8.3 cloudflared 経由で WebFinger が 404

→ Cloudflare Tunnel の Public Hostname 設定が `http://server:8080` を指していない可能性。cloudflared コンテナから `wget http://server:8080/.well-known/webfinger?...` で疎通確認。

### 8.4 TUI が server に繋がらない

→ Unix socket の権限。コンテナ内 nonroot (uid 65532) が書ける状態かを確認:

```bash
ls -ln /var/run/sakurasato-local/local.sock
# srwxr-xr-x 1 65532 65532 ... のはず
```

オプション A (bind mount) の手順をやり直す。

---

## 9. アンインストール

> ⚠️ **先にバックアップを取り、`secrets/` を別の安全な場所に控えてから実行する。**
> `secrets/postgres_password.txt` を失うと、`down -v` を omit して volume を残しても **postgres に接続不能** になり、データ復号は事実上不可能。順序を間違えると不可逆。

```bash
# 1. バックアップ (§7.2 の手順で postgres + versitygw + secrets + config)
mkdir -p ~/sakurasato-final-backup-$(date +%F)
cp -r secrets/ config/ ~/sakurasato-final-backup-$(date +%F)/
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml exec -T postgres \
  pg_dump -U sakurasato sakurasato | gzip > ~/sakurasato-final-backup-$(date +%F)/postgres.sql.gz

# 2. バックアップが揃ったことを目視確認 → コンテナと volume 削除
ls -la ~/sakurasato-final-backup-$(date +%F)/
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml down -v

# 3. ホスト側 secrets を消す (バックアップ済みのときだけ)
rm -f secrets/postgres_password.txt secrets/s3_secret_key.txt secrets/cloudflared_token.txt
```

`down -v` は **DB と versitygw volume も削除する** ので、データを残したいなら `-v` を外す。

---

## 関連ドキュメント

- [README.md](README.md) — プロジェクト概要
- [CLAUDE.md](CLAUDE.md) — 設計方針・実装計画・コーディング規約
