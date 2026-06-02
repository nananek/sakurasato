# Sakurasato サーバ CLI ガイド 🌸

> `sakurasato-server` バイナリのサブコマンド一覧と、それぞれの典型的な使い方。

Sakurasato は **Web の認証 UI を持たない** ため、ユーザ作成・トークン発行・絵文字インポート・鍵アカ管理・引っ越しといった管理操作は **サーバ側 CLI** で完結します。本ドキュメントは [`crates/server/src/cli.rs`](../crates/server/src/cli.rs) で定義される全コマンドのリファレンスです。

TUI 側の操作方法は [docs/TUI.md](TUI.md)、デプロイ手順は [DEPLOYMENT.md](../DEPLOYMENT.md) を参照してください。

---

## 0. 共通の前提

### 0.1 起動の形

実運用ではコンテナ内で `sakurasato-server` を呼び出します。docker compose 環境では:

```bash
docker compose -f docker-compose.yml -f docker-compose.ghcr.yml run --rm server <subcommand> [args]
```

開発時にローカルビルドで叩く場合:

```bash
cargo run -p sakurasato-server -- <subcommand> [args]
```

### 0.2 グローバル引数

| 引数 | 用途 |
|---|---|
| `--config <path>` | 追加 TOML overlay。`config/default.toml` の上に重ねる (DEPLOYMENT.md §3.3 参照) |

### 0.3 環境変数

設定はすべて `SAKURASATO_<セクション>__<キー>` 形式の env で上書き可能 (`__` がネスト区切り)。詳細は [DEPLOYMENT.md §3.3](../DEPLOYMENT.md) と [`crates/core/src/config.rs`](../crates/core/src/config.rs)。

---

## 1. サブコマンド一覧

| コマンド | 役割 | セクション |
|---|---|---|
| `serve` | AP 連合デーモンを起動 | [§2](#2-serve) |
| `init` | 初回 actor + 鍵生成 (DB マイグレーション込み) | [§3](#3-init) |
| `deliver` | キューにある特定の配送を 1 回手動で flush | [§4](#4-deliver) |
| `token` | ローカル API Bearer トークンの管理 | [§5](#5-token) |
| `emoji` | カスタム絵文字の管理 (Misskey 形式 zip インポート) | [§6](#6-emoji) |
| `alias` | `alsoKnownAs` の管理 (引っ越し受け入れ準備) | [§7](#7-alias) |
| `move-out` | フォロワー連れて他鯖へ引っ越し (Move 送出) | [§8](#8-move-out) |
| `follow` | acct で指定した相手に Follow を投入 | [§9](#9-follow) |
| `move-accept` | 受領済み Move 本文を CLI から再処理 | [§10](#10-move-accept) |
| `actor lock` / `unlock` | 鍵アカ運用切替 (M12 / #66) | [§11](#11-actor) |
| `follow-request` | 鍵アカ時の承認待ち follow の管理 | [§12](#12-follow-request) |
| `notification-channel` | Discord 互換 webhook 通知の宛先管理 | [§13](#13-notification-channel) |

---

## 2. `serve`

AP 連合デーモンを起動します。本番では compose 起動 (`docker compose up -d`) 経由で常駐させるのが普通で、CLI から直接叩く場面は少ないです (= ローカル開発で手動起動するとき用)。

```bash
sakurasato-server serve
```

---

## 3. `init`

DB マイグレーション + 単一ユーザ actor + 署名鍵 (RSA 2048 + Ed25519) の生成を行います。新規セットアップで **必ず一度** 走らせるコマンド。

```bash
sakurasato-server init
```

### 3.1 オプション

| 引数 | 既定値 | 用途 |
|---|---|---|
| `--username <name>` | `config.server.user` | 上書き用 (高度な用途のみ) |
| `--display-name <text>` | username と同じ | actor の表示名 |
| `--force` | `false` | 既存 actor を上書きして再 init (**フェデレーション破壊**) |
| `--locked` | `false` | 鍵アカ (`manuallyApprovesFollowers = true`) として初期化 |

### 3.2 `--force` の意味

`--force` は **鍵を再生成** します。これは:

- 既存フォロワーの inbox に送る配送が全て署名検証失敗で弾かれる
- ロールバック手段は postgres dump 復元のみ
- 緊急時 (= 鍵漏洩) のみ使う

詳細は [DEPLOYMENT.md §7.3 鍵ローテーション](../DEPLOYMENT.md)。

### 3.3 `--locked` と既存 lock 状態

- 新規 `init`: `--locked` あり → lock 状態で初期化 / 無し → unlock
- `init --force`: `--locked` あり → lock 維持/有効化 / 無し → **既存 lock は保たれる** (= unlock したい時は `actor unlock` を明示的に叩く)

これは「lock を片方向に倒すミス」(unlock 解除事故) を防ぐ設計です。

---

## 4. `deliver`

`delivery_queue` から 1 行を手動 flush します。配送ワーカが通常は自動 retry を回すので普段は不要ですが、**運用 retry の正規ルート** として残してあります (= 完全に裏で動かす設計ではなく、操作可能なエスケープハッチを残す方針)。

```bash
sakurasato-server deliver --queue-id <id>
```

### 4.1 想定ユースケース

#### (a) Follow 重複抑止後の明示 retry (Issue #113)

`sakurasato-server follow <acct>` は既存 pending 行があると **新規 enqueue を抑止** します (= 同じ Follow が相手側で累積するのを防ぐため)。元の `delivery_queue` 行が失敗していて再送したいときは:

```bash
# 1) 失敗中の Follow を探す
psql ... -c "SELECT id, target_inbox, status, last_error FROM delivery_queue \
             WHERE status IN ('pending','failed') ORDER BY id DESC LIMIT 20;"

# 2) 該当 id を再 flush
sakurasato-server deliver --queue-id 42
```

#### (b) worker の retry 上限到達後の復活

配送ワーカは指数バックオフで一定回数 retry した後 `status = 'failed'` で諦めます。相手側の長期障害が回復した後は本 CLI で再投入します:

```bash
psql ... -c "SELECT id FROM delivery_queue WHERE status = 'failed';"
sakurasato-server deliver --queue-id 42
```

### 4.2 queue 行の手動クリーンアップ

完全に諦めた `failed` 行を一括掃除したいときは psql で直叩き:

```sql
-- 30 日以上前の failed 行を全削除 (お一人様サーバの整備)
DELETE FROM delivery_queue
 WHERE status = 'failed'
   AND created_at < now() - interval '30 days';
```

専用 CLI は意図的に持たず、`psql` 直叩きを運用前提にしています (= 攻撃面を増やさない / 削除タイミングは運用者判断)。

### 4.3 一覧の取得

`queue_id` を列挙する CLI は意図的に用意していません。`psql` で直接見るのが正規ルート:

```bash
psql ... -c "SELECT id, target_inbox, status, last_attempt_at, last_error \
             FROM delivery_queue \
             WHERE status != 'delivered' \
             ORDER BY id DESC LIMIT 20;"
```

---

## 5. `token`

TUI / pytest など Bearer 認証でローカル API を叩くクライアント用のトークン管理。

### 5.1 `token issue`

新規発行。**生 token は stdout に一度きり** 表示されます (DB にはハッシュのみ保管)。

```bash
sakurasato-server token issue --name "tui-laptop"
```

ファイル出力 (= compose の named volume で TUI / pytest に共有する用途):

```bash
sakurasato-server token issue --name "ci" --out /run/sakurasato-secrets/ci-token
```

`--out` 指定時、**既存ファイルが存在すると失敗** します (= 古いトークンが意図せず奪われるのを防ぐ)。

### 5.2 `token list`

発行済みトークンの一覧 (id / name / created / last_used)。**生 token は再表示できません** ── 失念したら `revoke` + `issue` で作り直し。

```bash
sakurasato-server token list
```

### 5.3 `token revoke`

指定 id を hard-delete。

```bash
sakurasato-server token revoke --id 3
```

---

## 6. `emoji`

カスタム絵文字管理。M8 で Misskey 形式 zip インポートのみ実装。

### 6.1 `emoji import`

Misskey の export 形式 zip (`meta.json` + 画像ファイル) を取り込みます。

```bash
sakurasato-server emoji import /path/to/misskey-emoji.zip
```

- 既存 shortcode は **上書き** されます (CLAUDE.md §5.4)
- 画像バイト列は **server 本体ではデコードしない** ── media-proxy の `/v1/image/sanitize` 経由で再エンコードしてから versitygw に格納 (CLAUDE.md §7 隔離方針)
- `downloaded == true` の絵文字のみ取り込み (Misskey の元仕様準拠)

---

## 7. `alias`

`alsoKnownAs` の管理。他鯖から **引っ越し受け入れ** をするとき、こちら側の actor に「自分が以前居た場所」を宣言する必要があります (Mastodon の双方向同意検査用)。

### 7.1 `alias list`

```bash
sakurasato-server alias list
```

### 7.2 `alias add`

URI を追加 (冪等)。actor `Update` を全フォロワーに配信します。

```bash
sakurasato-server alias add https://old.example.com/users/me
```

### 7.3 `alias remove`

URI を削除 (無い場合は no-op)。同じく `Update` 配信。

```bash
sakurasato-server alias remove https://old.example.com/users/me
```

### 7.4 `alias clear`

全エントリクリア + `Update` 配信。

```bash
sakurasato-server alias clear
```

---

## 8. `move-out`

フォロワーを連れて別 actor へ **引っ越し送出**。Mastodon / Misskey と同じ作法で、Move activity を全フォロワーに送り、相手側に follow を引き継いでもらいます。

```bash
sakurasato-server move-out https://new.example.com/users/me
```

### 8.1 双方向同意検査

**移動先 actor の `alsoKnownAs` に自分の `ap_id` が先に登録されていない** と CLI は拒否します。これは「勝手に他人の actor に引っ越し偽装する」のを防ぐためで、Mastodon も同じ作法です。

移動先で先に登録してもらった上で `move-out` を実行する流れ:

1. 移動先 actor の側で `alsoKnownAs` に Sakurasato の actor URI を追加
2. Sakurasato 側で `move-out <移動先 URI>` を実行

### 8.2 `--force` (緊急時のみ)

双方向検査をスキップします。同意が無い状態で Move を投げると相手側で偽装と扱われる可能性が高いので、**通常は使わない**。

```bash
sakurasato-server move-out https://new.example.com/users/me --force
```

---

## 9. `follow`

acct で指定した相手に Follow を投入します。WebFinger 解決 (media-proxy 経由) → actor URI 取得 → `delivery_queue` に Follow を 1 行積みます。常駐ワーカが拾って送出。

```bash
sakurasato-server follow acct:alice@misskey.example
# または
sakurasato-server follow @alice@misskey.example
sakurasato-server follow alice@misskey.example
```

### 9.1 WebFinger をスキップ (actor URI 直指定)

```bash
sakurasato-server follow --actor-uri https://misskey.example/users/abcd1234
```

`acct` 引数があっても **`--actor-uri` が優先** されます。

### 9.2 冪等

同じ相手に何度叩いても `(follower, followed)` UNIQUE 制約 + 決定論的 activity id で冪等。既存 row が:

- `accepted` → no-op
- `rejected` → 明示拒否
- `pending` → 再 enqueue (= retry)

---

## 10. `move-accept`

**初回受領時に DB / network エラーで 503 を返した** inbound Move を CLI から手動再処理するための薄いラッパ。HTTP 署名検証はスキップされるため、**自分が控えておいた activity 本文 (= 通常経路で受領したものを保存しておいた JSON)** にのみ使ってください。

```bash
sakurasato-server move-accept --from /path/to/saved-move-activity.json
```

### 10.1 signer 上書き

通常は activity 本文の `actor` を信用しますが、改竄を疑うときに上書き可能:

```bash
sakurasato-server move-accept --from /path/to/saved.json --signer https://x.example/users/alice
```

### 10.2 安全側のガード

`type == "Move"` のみ受け付けます (それ以外は拒否)。さらに `handle_move` 内で:

- `alsoKnownAs` 双方向同意検査
- target actor の fresh fetch

を行うので、第三者から渡された JSON を流しても勝手に follow が向こう側に倒れることは無い設計です。それでも **未検証 JSON を流すのは推奨しません**。

---

## 11. `actor`

鍵アカ運用 (`manuallyApprovesFollowers = true`) の切替。M12 / Issue #66。

### 11.1 `actor lock`

鍵アカ化。

```bash
sakurasato-server actor lock
```

- actor JSON が `manuallyApprovesFollowers: true` を emit するようになる
- フォロワー全員に actor `Update` を配信 (= 相手側のキャッシュを更新)
- 新規 Follow は `pending` に据え置かれ、`follow-request approve/reject` で明示的に処理する必要がある
- **既存 `accepted` フォロワーが Mastodon 側で Follow を retry してきた** ケースは引き続き Accept が自動で返る (= `:lock` した瞬間に従来フォロワーを切るのではなく、新規 Follow だけ承認制に切替える設計)

### 11.2 `actor unlock`

鍵アカ解除。

```bash
sakurasato-server actor unlock
```

- 同様に actor `Update` を配信
- **lock 中に溜まった pending Follow は auto-Accept されません** ── 明示的に `follow-request approve/reject` する必要があります (Mastodon と同じ作法 / unlock 事故防止)

### 11.3 TUI から触る

M12 (PR #95) で TUI command mode にも同等コマンドが入っています:

| TUI command | サーバ CLI 同等 |
|---|---|
| `:lock` | `actor lock` |
| `:unlock` | `actor unlock` |
| `:requests` | `follow-request list` (+ 画面内で `a` / `x` で approve / reject) |

詳細は [docs/TUI.md §4 Command mode](TUI.md)。

---

## 12. `follow-request`

鍵アカ中の承認待ち follow を CLI から処理。

### 12.1 `follow-request list`

`follow.state = 'pending'` かつ followed が local actor の行を列挙。`id` / `follower (ap_id)` / `received_at` を出します。

```bash
sakurasato-server follow-request list
```

### 12.2 `follow-request approve`

指定 id を承認 → Accept activity を `delivery_queue` に積み + `follow.state = 'accepted'` に遷移。

```bash
sakurasato-server follow-request approve --id 42
```

### 12.3 `follow-request reject`

指定 id を拒否 → Reject activity を積み + `follow.state = 'rejected'` に遷移。

```bash
sakurasato-server follow-request reject --id 42
```

---

## 13. `notification-channel`

Discord (および Slack / Misskey 互換) webhook で push 通知する宛先を管理します。お一人様サーバには Web UI が無いので、外出中に「自分宛の何かが来た」ことを Discord 等で気付くための経路。**配送経路は既存 `delivery_queue` を流用** ── retry / backoff / dead 状態機械を AP 配送と共有します。`activity.type` が `Webhook:` prefix の行は worker が HTTP 署名を skip して `application/json` で POST します。

### 通知発火イベント (7 種)

| event 名 | 発火元 |
|---|---|
| `mention` | 自分が `tag.Mention` に乗った Note を受信 |
| `direct` | `to` に自分の actor URI のみが指定された Note (= 自分宛 DM) |
| `quote` | 自分の Note を `quote` した Note を受信 |
| `reaction` | 自分の Note への `Like` / `EmojiReact` |
| `renote` | 自分の Note への `Announce` (= boost / renote) |
| `follow` | 自分への `Follow` が `accepted` 状態で着地 |
| `follow-request` | 鍵アカ運用時に自分への `Follow` が `pending` 状態で着地 |
| `all` | 上記 7 種の一斉セット (`enable` / `disable` 時のみ意味を持つ) |

### 13.1 `notification-channel add`

Webhook URL を登録します。`--format` は省略時 `embed` (= Discord 互換)。

```bash
# 1) Discord のチャンネル設定 → 連携サービス → Webhook で URL を取得
# 2) 登録 (--format 省略時は embed)
sakurasato-server notification-channel add \
  --name discord-personal \
  --url 'https://discord.com/api/webhooks/123.../abc...' \
  --format embed
```

- `--name` は表示・CLI 識別用ラベル (DB UNIQUE)。同名再登録は拒否される。
- `--url` は登録時に SSRF ガード ([`net_guard::host_blocked`](../crates/server/src/net_guard.rs)) で検査します。private / loopback / link-local / reserved の宛先は弾かれます。配送時にも DNS 再解決後の TOCTOU 防御で再検査されます。
- `--format`: `embed` (Discord embed JSON) / `plain` (`{"content": "..."}` で Slack の `text` フィールドや Misskey 互換 fallback と相互運用)。

登録後の初期状態は **7 イベントすべて ON**。「フォローだけ通知したい」場合は `enable --id N --only follow` の一発で完全宣言できます。あるいは `disable --id N all` で一旦すべて OFF にしてから `enable --id N follow` で必要な分だけ ON に戻す運用も可。

### 13.2 `notification-channel list`

登録チャンネルを 1 行ずつ列挙します。**URL は host だけ表示** ── URL 全体が capability であり、screenshot や paste で漏らされるのを避けるため。

```bash
sakurasato-server notification-channel list
# 例:
# id=1 name="discord-personal" format=embed host=discord.com events=mention,direct,quote,reaction,renote,follow,follow-request
```

`events` 列がそのチャンネルで通知発火する event の一覧 (= `notify_<event> = TRUE` な列を抽出)。完全な URL が必要な場合は `psql` で `SELECT url FROM notification_channel WHERE id = N;` を叩いてください。

### 13.3 `notification-channel enable` / `disable`

`notify_<event>` を **ON** / **OFF** に設定します (idempotent ── 同じコマンドを再実行しても結果は変わらず DB 状態は等しい)。event は **positional 引数で 1 個以上**、空白区切りでも `,` 区切りでも、混在でも可。

```bash
# 個別 event の有効化 (= partial update / 他列据え置き)
sakurasato-server notification-channel enable  --id 1 mention
sakurasato-server notification-channel disable --id 1 reaction

# 複数 event の一括有効化 / 停止 (1 UPDATE 文で atomic)
sakurasato-server notification-channel enable  --id 1 mention,quote
sakurasato-server notification-channel disable --id 1 mention reaction renote
sakurasato-server notification-channel enable  --id 1 mention,quote follow

# チャンネル全停止 (= 7 個の notify_* を一斉 FALSE)
sakurasato-server notification-channel disable --id 1 all
# チャンネル全有効化 (= 7 個の notify_* を一斉 TRUE)
sakurasato-server notification-channel enable  --id 1 all
```

#### `--only`: フル状態宣言モード (enable のみ)

`enable --only A,B` は「A と B だけ ON、他は OFF」── 希望状態を 1 コマンドで言い切ります。スクリプトや ansible で「現状を問わず状態 X に揃える」用途。完全冪等。

```bash
# mention と quote だけ ON、他 5 列は OFF
sakurasato-server notification-channel enable --id 1 --only mention,quote

# follow だけ ON、他 6 列は OFF (= 「フォロー通知だけ欲しい」)
sakurasato-server notification-channel enable --id 1 --only follow
```

- `--only` は **`enable` 限定** (= `disable --only` は提供しない。「X だけ OFF」は `enable --only (complement)` 経由で書ける + 「全部 OFF」は `disable all` で済むため)。
- `--only all` は意味が無いので拒否 (`enable --id N all` を使ってください)。
- `all` を `--only` 無しで指定するのと `--only` 付きで指定するのは結果が同じ ── 後者は CLI で reject する設計。

#### 設計メモ

- 旧バージョンには `toggle` サブコマンドと `enabled` master 列がありましたが、`--event all` が「7 個一斉反転」ではなく「master 反転」を意味し直感に反していたこと、`toggle` が冪等にならないこと (= スクリプトから安全に呼べない) から、migration 0014 で master 撤去 + `enable` / `disable` への置換が行われました。
- 通知 fan-out は `WHERE notify_<event> = TRUE` の単純フィルタです (master の AND 条件は無くなりました)。
- 不明な event 名 (typo 等) は CLI で早期 `bail!` ── DB を一切触らずに失敗します。受理する token: `mention` / `direct` / `quote` / `reaction` / `renote` / `follow` / `follow-request` / `all`。

### 13.4 `notification-channel test`

固定文言のテスト通知を 1 件 `delivery_queue` に enqueue します。実 POST は worker のティック次第 (即時ではない)。

```bash
sakurasato-server notification-channel test --id 1
```

embed なら `title: "テスト通知"` + 説明文、plain なら `[テスト通知] ...` の `content`。Follow event のペイロード形を流用しているので、表示は「フォロー通知」ではなく明示的に「テスト通知」になります。届かない場合は:

- `delivery_queue` の該当行の `last_error` を `psql` で確認
- channel の `notify_*` がすべて FALSE になっていないか `list` で確認 (= 全 OFF だと dispatch 側で `WHERE notify_<event> = TRUE` に引っかからず通知が出ない)
- 通知経路自体は届くか別 event で切り分け: 例えばテスト用に手元から自分の actor へ Follow を投げてみて (= 別端末 / 別アカウント経由) `follow` event が発火するか観察。`enable --id N follow` で対象 event が ON である前提

### 13.5 `notification-channel remove`

`--id` 指定のハード削除。配送中の `delivery_queue` 行には影響しません (= 既に enqueue 済みの通知は worker が撃ち切る)。

```bash
sakurasato-server notification-channel remove --id 1
```

---

## トラブルシューティング

### TUI から `:lock` を叩いたが反映されない

サーバ CLI と同じ local API (`POST /api/v1/actor/lock`) を叩いているので、原理上挙動は同じです。`server` のログ (`docker compose logs -f server`) で `actor_admin` の error を確認してください。

### `init --force` で間違って鍵を再生成してしまった

postgres の dump 復元しかありません ([DEPLOYMENT.md §7.2 バックアップ](../DEPLOYMENT.md))。バックアップが無い場合、**全フォロワー関係は失われます**。新規鍵で再フォローしてもらうしかありません。

### `follow` が pending のまま遷移しない

- 相手側が鍵アカ (= `manuallyApprovesFollowers = true`) で承認待ち状態
- 相手の管理者に承認依頼するか、`follow-request approve` 待ち
- `delivery_queue` のステータスを `psql` で確認

### `move-out` が拒否される (双方向同意検査)

移動先 actor の `alsoKnownAs` に **先に** 自分の ap_id を入れてもらう必要があります。例: Mastodon → Sakurasato の引っ越しなら、まず Mastodon 側 `tootctl accounts merge` か Web UI で alsoKnownAs を設定。詳細は §8.1。

### Discord 通知が届かない

順に確認:

1. `notification-channel list` で対象チャンネルが存在し、`events` 列に対象 event が含まれているか確認 (= `notify_<event> = TRUE` か):
   - そもそも `list` に出ないなら未登録 → `notification-channel add` で先に作る
   - 出るが `events=(none)` なら全 event が OFF → `notification-channel enable --id N all` で復帰
   - 出るが対象 event だけ抜けているなら → `notification-channel enable --id N <event>` で個別 ON
2. Webhook URL の host が SSRF ガードで遮断されていないか (登録時に弾かれていれば `add` で失敗していますが、運用中に DNS が private IP に倒れたケースは配送時 TOCTOU で弾かれます)。`delivery_queue.last_error` を `psql` で確認。
3. `delivery_queue.state = 'dead'` で停止していないか。`sakurasato-server deliver --queue-id N` で手動 flush して挙動を確認。
4. `notification-channel test --id N` でテスト通知を 1 件撃って、worker tick のタイミングと配送経路だけ切り分け。

---

## 関連ドキュメント

- [docs/TUI.md](TUI.md) ── TUI クライアント操作ガイド
- [DEPLOYMENT.md](../DEPLOYMENT.md) ── サーバ本番デプロイ手順
- [CLAUDE.md](../CLAUDE.md) ── アーキテクチャ・設計方針
- [`crates/server/src/cli.rs`](../crates/server/src/cli.rs) ── CLI 定義 (本ドキュメントの ground truth)
