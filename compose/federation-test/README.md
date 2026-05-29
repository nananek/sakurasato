# Federation Test Stacks

CI 外の手動 e2e 用 compose 一式。sakurasato の HTTP 署名検証 (受信側) と
配送 (送信側) が本物の他実装相手にどう動くかを試すための踏み台。

## 提供スタック

| impl | image | 主用途 | 備考 |
|---|---|---|---|
| `mastodon` | `ghcr.io/mastodon/mastodon:v4.3` | cavage RSA-SHA256 主対向 | 公式 image。`bob/Password1234!` |
| `misskey` | `misskey/misskey:2025.2.1` | cavage RSA-SHA256 検証 | 初回起動時に Web UI で admin 作成 |
| `pleroma` | `ghcr.io/explodingcamera/pleroma:stable` | cavage RSA-SHA256 検証 | 登録 open。Web UI から登録 |
| `mitra` | `bleakfuture0/mitra:v5.4.0` | **RFC 9421 + Ed25519** | FEP-521a Multikey 対応。`bob/password123` |
| `fedibird` | inline build (`#fedibird` ブランチ) | Mastodon fork の cavage RSA | 初回ビルド 30 分強 |
| `nekonoverse` | inline build (`#develop`) | **RFC 9421 + Ed25519** 主対向 | dual-key 完備、自実装ペア |

Mastodon / Misskey / Fedibird の公式 actor JSON は `assertionMethod` を
持たない (= RSA のみ公開) ので、Ed25519 経路を本物相手に試したい場合は
**Mitra か Nekonoverse** を使う。

## 起動 / 停止

```bash
# 起動
./scripts/federation-test/up.sh mastodon

# 動作確認 (URL / curl / ログ表示)
./scripts/federation-test/setup-mastodon.sh

# 停止 (volume も削除)
./scripts/federation-test/down.sh mastodon
```

`mastodon` を `misskey / pleroma / mitra / fedibird / nekonoverse` の
いずれかに差し替えて使う。

## /etc/hosts

ブラウザから直接触りたい場合のみ追加:

```
127.0.0.1 sakurasato mastodon misskey pleroma mitra fedibird nekonoverse
```

CLI からの `curl` だけで済ますなら `--resolve sakurasato:443:127.0.0.1`
で十分。

## アーキテクチャ

```
                       ┌─ certs (alpine + openssl)
                       │   - shared CA + per-host certs (1 day validity)
                       ▼
   ┌─────────────────┬─────────────────────┐
   │  sakurasato 側   │   <impl> 側           │
   │  ─────────────  │   ─────────────       │
   │  postgres-sks   │   postgres-<x>        │
   │  sakurasato-    │   (redis/valkey)      │
   │   init  (1-shot)│   <impl>-app          │
   │  sakurasato-    │   <impl>-worker (任意)│
   │   server         │                      │
   │  nginx-sks      │   nginx-<x>           │
   │  alias:          │   alias: <impl>       │
   │   sakurasato     │                      │
   └─────────────────┴─────────────────────┘
              共通: <impl>-fed network
```

各 compose ファイルは独立 (network も volume も別)。同時に複数立ち上げて
OK。

### sakurasato 側 (全 compose で共通)

- `certs` で発行した `/certs/{sakurasato.crt,sakurasato.key,ca.crt}` を
  `nginx-sks` でマウントし TLS 443 終端
- `sakurasato-init` (one-shot) が DB migrate + actor 鍵生成
- `sakurasato-server` (常駐) が `:8080` で待ち受け、`nginx-sks` から proxy
- 外向き reqwest は `SSL_CERT_FILE=/certs/ca.crt` で test CA を信頼
- 配送 worker は M3b-2 PR2 時点では未常駐 (`sakurasato deliver --queue-id`
  で手動 flush)

### <impl> 側

各 impl は test CA を `/usr/local/share/ca-certificates/` にコピーしてから
本体を起動 (entrypoint)。これで impl → sakurasato の TLS が通る。

## M3b-2 PR3 時点で検証できる範囲

PR3 マージ時点では sakurasato の Follow / Accept ハンドラは未実装
([[m3b-followup-plan]] 参照)。以下は確認可能:

- 各 impl から sakurasato actor JSON / WebFinger 取得 → 200 OK
- 各 impl から sakurasato `/inbox` に Follow POST →
  cavage RSA 署名検証が通り、**未知 actor として 401**
  (= 検証ロジックが正しく動いている証拠)
- nekonoverse / mitra から sakurasato `/inbox` に **Ed25519 署名付き POST** →
  401 (= RFC 9421 検証パス通過のサイン)
- 逆方向: `sakurasato deliver --queue-id <id>` で対向 inbox に POST →
  各 impl のログで cavage RSA-SHA256 受理確認

完全な相互フォロー成立は M3b-3 (remote actor fetch + Follow handler) 完了後。

## トラブルシュート

| 症状 | 原因 | 対処 |
|---|---|---|
| `sakurasato-init` が `database does not exist` | postgres まだ起動中 | depends_on の healthcheck 通過待ち。再 up |
| 対向側で `SSL: certificate verify failed` | CA cert の trust が反映されていない | impl の entrypoint で `update-ca-certificates` が走るログを確認 |
| `nginx-sks: connect() failed (111)` | sakurasato-server がまだ listen していない | server ログで `sakurasato-server listening` 確認 |
| Fedibird の build が遅い | gem + yarn フルビルド | 初回 30 分強。次回以降は cache hit |
| `port 443 already in use` | 他 compose スタックが起動中 | down.sh で前のスタックを落とす (各 compose は独立 network なので 443 は host に公開していないはずだが、念のため) |

## CI から除外されている理由

- `docker compose build` だけで GB 単位のディスクと数十分を食う
- 安定性が impl 側 image の更新タイミングに引きずられる
- nekonoverse でも同様に CI 外で運用

`.github/workflows/ci.yml` の `paths-ignore` に `compose/docker-compose.federation-*.yml`
/ `compose/federation-test/**` / `scripts/federation-test/**` を追加して PR が CI を
トリガーしないようにしている。`codeql.yml` は元から `**/*.rs` と `**/Cargo.*` の
`paths` 制限で `compose/` を対象外にしているため変更不要。
