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

再 keying は `init --force`。**フォロワーへの配送が全部署名検証失敗で弾かれる**ので緊急時のみ。

### 3.6 起動

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml up -d
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml ps
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml logs -f server
```

---

## 4. Cloudflare Tunnel の設定

ホスト機の `127.0.0.1:8080` を Cloudflare Tunnel に渡す。`docker-compose.yml` の `server` サービスは内部ネットだけだが、`docker-compose.dev.yml` 同様の overlay でホスト bind しても、cloudflared を **同じ compose の追加サービス** として動かしてもよい。後者の例:

```yaml
# docker-compose.cloudflared.yml (例)
services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    restart: unless-stopped
    command: tunnel --no-autoupdate run --token "${CLOUDFLARED_TOKEN}"
    networks:
      - egress
      - internal
    depends_on:
      - server
```

cloudflared ダッシュボードで:
- **Public Hostname** → `sakurasato.example.com`
- **Service** → `http://server:8080`

を登録する。タイムアウト・HTTP/2 設定は標準で問題なし。

### 4.1 動作確認

```bash
curl -sv "https://sakurasato.example.com/.well-known/webfinger?resource=acct:me@sakurasato.example.com" | head
curl -sv "https://sakurasato.example.com/users/me" | head
```

actor JSON が返れば連合準備完了。

---

## 5. TUI 経路 (Tailscale)

`docker-compose.yml` の `server` サービスは `/run/sakurasato-local/local.sock` を `local_sock` named volume にマウントしている。デフォルトではコンテナ内 nonroot (= uid 65532) しか書けないので、TUI ホストから直接マウントするには 1 工夫要る。

### オプション A: ホスト bind mount

`docker-compose.yml` の `server.volumes` を bind mount に切り替え、ホスト側で chown する。Tailscale ノードが同じホストならこれで十分。

```yaml
# docker-compose.tui.yml (例)
services:
  server:
    volumes: !override
      - media_sock:/run/sakurasato
      - /var/run/sakurasato-local:/run/sakurasato-local
```

```bash
sudo mkdir -p /var/run/sakurasato-local
sudo chown 65532:65532 /var/run/sakurasato-local
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml -f docker-compose.tui.yml up -d
```

TUI 端末から:

```bash
ssh sakurasato-host.tailnet -- cargo run -p sakurasato-tui    # 開発時
# または ghcr 経由で TUI バイナリを取得して
ssh sakurasato-host.tailnet -- sakurasato-tui --socket /var/run/sakurasato-local/local.sock
```

### オプション B: tailscale serve でブリッジ

`tailscale serve` でホスト機の Unix socket を tailnet 内向け HTTPS に晒す。TUI 側は HTTPS を叩く。実装上は **`/api/v1/*` が public listener に登録されていない**（CLAUDE.md §5.1 / `serve.rs` の二段 listener）ので、socket → tailnet bridge を作る方が筋がよい。

```bash
# ホスト機
tailscale serve --bg --tcp 8443 /var/run/sakurasato-local/local.sock
```

TUI 側は `https://sakurasato-host.tailnet:8443` を叩く設定にする（要 Bearer トークン）。

### 5.1 トークン発行

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server token issue --name tui-laptop
# 出力された raw token を TUI 側に控える (この一度しか表示されない)
```

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
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml exec postgres \
  pg_dump -U sakurasato sakurasato | gzip > backup-$(date +%F).sql.gz
```

### 7.3 鍵ローテーション

**通常は触らない**。万一秘密鍵が漏れた場合のみ:

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server init --force
```

実行すると新しい RSA / Ed25519 鍵ペアが生成され、`public_key_id` も変わる。**既存フォロワーは旧鍵 ID をキャッシュしているので、相手側で actor JSON が refresh されるまで配送が全部 401 で弾かれる**。再フォローを依頼する覚悟が必要。

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

**`move-accept` は HTTP 署名検証を通らない**ので、自分が控えておいた本文でのみ実行すること。

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
1. peer 側で actor JSON の cache を refresh してもらう (Mastodon なら admin)
2. または `delivery_queue` の `state = 'failed'` 行を `pending` に戻して再送

```sql
UPDATE delivery_queue SET state='pending', retries=0, next_attempt_at=now() WHERE state='failed';
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

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml down -v
rm -rf secrets/postgres_password.txt secrets/s3_secret_key.txt
```

`down -v` は **DB と versitygw volume も削除する** ので、データを残したいなら `-v` を外す。

---

## 関連ドキュメント

- [README.md](README.md) — プロジェクト概要
- [CLAUDE.md](CLAUDE.md) — 設計方針・実装計画・コーディング規約
