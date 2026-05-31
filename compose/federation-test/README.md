# Federation Test Stacks

CI 外の手動 e2e 用 compose 一式。sakurasato の HTTP 署名検証 (受信側) と
配送 (送信側) が本物の他実装相手にどう動くかを試すための踏み台。

## 提供スタック

| impl | image | 主用途 | 備考 |
|---|---|---|---|
| `mastodon` | `ghcr.io/mastodon/mastodon:latest` | cavage RSA-SHA256 主対向 | 公式 image。`bob/Password1234!` |
| `misskey` | `misskey/misskey:latest` | cavage RSA-SHA256 検証 | `POST /api/admin/accounts/create` で admin 作成 (初回のみ無認証で通り、レスポンスの `token` を以降の auth に使う) |
| `pleroma` | `ghcr.io/explodingcamera/pleroma:stable` | cavage RSA-SHA256 検証 | 登録 open。Web UI から登録 |
| `mitra` | `bleakfuture0/mitra:latest` | **RFC 9421 + Ed25519** | FEP-521a Multikey 対応。`bob/password123` |
| `fedibird` | inline build (`#fedibird` ブランチ) | Mastodon fork の cavage RSA | 初回ビルド 30 分強 |
| `nekonoverse` | `ghcr.io/nekonoverse/nekonoverse-backend:latest` | **RFC 9421 + Ed25519** 主対向 | dual-key 完備、自実装ペア |

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

### Programmatic (pytest) — Mastodon のみ (M12 / Issue #56)

手動 `setup-*.sh` の代わりに pytest で Pass/Fail を出すモード:

```bash
./scripts/federation-test/pytest.sh mastodon
# ↑ compose --profile pytest で全サービスを立ち上げ、
#   `tests/federation/test_mastodon.py` を回す
# DEBUG_KEEP=1 を付けると終了後もコンテナを残す (= ログ漁り用)
```

CI では `.github/workflows/federation-test.yml` が nightly cron +
`workflow_dispatch` で同じ stack を回す (required check ではない、
外部 image の更新で揺らぐため)。

後続 PR で Misskey / Pleroma / Mitra / Fedibird / Nekonoverse 用の
`test_misskey.py` 等を同じパターンで追加する予定。

## 検証はコンテナの中から

各 compose は **ホストに 443 を公開しない** (impl 同士が内部ネット上の DNS
alias で直接話す)。動作確認は `docker compose ... exec <impl-side>` 経由で
コンテナの中から `curl` を叩く:

```bash
docker compose -f compose/docker-compose.federation-mastodon.yml \
  exec mastodon-web curl -sk \
  'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato'
```

ホスト側で `--resolve sakurasato:443:127.0.0.1` をやりたい場合は compose に
`ports: ["443:443"]` を一時的に足す (定常運用には不要)。

## /etc/hosts

ブラウザから直接触りたい場合のみ追加:

```
127.0.0.1 sakurasato mastodon misskey pleroma mitra fedibird nekonoverse
```

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
- 配送 worker は **常駐** (M3b-3 PR2 以降)。inbox から受領した Follow に
  対し非同期で Accept を投げ返す。
- ローカル API ソケットは `/tmp/sakurasato-local.sock` に逃がしている
  (compose 既定の `/run/sakurasato/` は rootless container では書けない)。
  federation-test では TUI を使わないので問題なし。

### <impl> 側

各 impl は test CA を `/usr/local/share/ca-certificates/` にコピーしてから
本体を起動 (entrypoint)。これで impl → sakurasato の TLS が通る。

## 現状検証できる範囲 (M3b-3 完了後)

- 各 impl から sakurasato actor JSON / WebFinger 取得 → 200 OK
- 各 impl から sakurasato `/inbox` に Follow POST → cavage RSA 署名検証 →
  Follow handler 起動 → Accept キュー投入 → delivery worker が impl inbox に
  POST → 受理 → impl 側に Follow 関係成立
- 対応状況 (2026-05-30 時点):
  - **mastodon / mitra / fedibird / misskey**: 完全相互フォロー成立
  - **pleroma**: cavage RSA 署名検証で 401 (M3b フォロー対象)
  - **nekonoverse**: cavage に Ed25519 keyId を載せる流派で sakurasato が
    拒否 (M3b フォロー対象)

### M9: Move (引っ越し) の手動検証

Mastodon は `tootctl account move <FROM> <TO>` で Move activity を全フォロワー
inbox に投げてくれるので、これを使って sakurasato 側の受領経路を検証できる。

```bash
# 1) Mastodon スタック起動 + sakurasato 側で bob@mastodon を follow しておく。
./scripts/federation-test/up.sh mastodon

# 2) Mastodon 側に移動先 alice を作る (Web UI の Register Account でも、
#    tootctl でも良い)。
docker compose -f compose/docker-compose.federation-mastodon.yml \
  exec mastodon-web tootctl accounts create alice \
  --email alice@mastodon --confirmed

# 3) alice の Web UI 設定 → "Move from a different account" で
#    bob@mastodon を旧 actor として登録 (alsoKnownAs に bob が載る)。
#    あるいは sakurasato 側で `sakurasato alias add` を打って bob → alice
#    の片方向 alsoKnownAs を入れる手もある (実際の引っ越しは前者推奨)。

# 4) bob 側で account move を実行。
docker compose -f compose/docker-compose.federation-mastodon.yml \
  exec mastodon-web tootctl accounts move bob alice@mastodon

# 5) sakurasato のログで Move handler の起動と auto-Follow の queue を確認。
docker compose -f compose/docker-compose.federation-mastodon.yml \
  logs sakurasato-server | grep -E '(Move accepted|auto-Follow queued)'
```

`alsoKnownAs` 側だけ試したい (= Move まで打たない) 場合は CLI を使う:

```bash
docker compose -f compose/docker-compose.federation-mastodon.yml \
  exec sakurasato-server sakurasato alias add https://old.example/users/me
docker compose -f compose/docker-compose.federation-mastodon.yml \
  exec sakurasato-server sakurasato alias list
```

### Misskey 注意点 (2026.5+)

`meta.federation` のデフォルトが `'none'` (連合無効) に変わったので、
`up.sh misskey` 後に必ず `setup-misskey.sh` を流す:

```bash
./scripts/federation-test/up.sh misskey
./scripts/federation-test/setup-misskey.sh   # admin 作成 + federation 有効化 + 再起動
```

`setup-misskey.sh` は admin (`alice/Password1234!`) を作成し、
`UPDATE meta SET federation = 'all'` を流して misskey-app を再起動する。

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
