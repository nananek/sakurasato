# Sakurasato TUI 操作ガイド 🌸

> 端末の中の静かな隠れ家を、実際に動かす方法。

Sakurasato は **TUI 専用クライアント** で操作する Fediverse サーバです。本ドキュメントは TUI バイナリ `sakurasato-tui` の起動方法・画面構成・キー操作・コマンド一覧をまとめています。

サーバ自体のセットアップは [DEPLOYMENT.md](../DEPLOYMENT.md) を、設計方針は [CLAUDE.md](../CLAUDE.md) を参照してください。

---

## 0. 前提

- **端末**: [Kitty](https://sw.kovidgoyal.net/kitty/) / [WezTerm](https://wezfurlong.org/wezterm/) / iTerm2 など Kitty graphics protocol 対応端末を推奨。Sixel / iTerm2 inline image protocol もフォールバック対応 (`ratatui-image` 自動検出)。非対応端末ではテキスト UI に倒れる。
- **マウス**: クリック / ホイール / ドラッグ対応 (`crossterm` のマウスイベント)。
- **サーバ**: `sakurasato-server` が起動済みで `local_api_socket` (= `/run/sakurasato-local/local.sock`) または `local_api_listen` (TCP) を listen していること。
- **トークン**: ローカル API は Bearer 認証。`sakurasato-server token issue` で発行する。raw token は一度きり stdout 表示。

---

## 1. 起動

### 1.1 接続先

TUI と server は **Unix socket** または **TCP** で繋ぐ。優先順位は `--api-url` > `--socket` > 既定パス (`/run/sakurasato/local.sock`)。

```bash
# UDS (同じホスト上で TUI を直接動かす場合 — 最も素直)
sakurasato-tui

# TCP (Tailscale 等を介して別端末から触る場合 — DEPLOYMENT.md §5)
sakurasato-tui --api-url http://hostname.<tailnet>.ts.net:18080
```

`--api-url` は `http://` のみ受理。`https://` を渡すと「TUI に TLS スタック無し」エラーで明示拒否する (Tailscale tailnet 内 = WireGuard で既に暗号化されている前提)。

### 1.2 トークン

3 通り。**`SAKURASATO_TOKEN` env を推奨**。

| 方法 | 例 | メモ |
|---|---|---|
| env | `SAKURASATO_TOKEN=xxx sakurasato-tui` | プロセス引数に出ない |
| ファイル | `sakurasato-tui --token-file ~/.config/sakurasato/token` | 改行は trim |
| 直書き | `sakurasato-tui --token xxx` | `ps` / `docker inspect` から見えるので非推奨 (起動時 warn) |

サーバ側:

```bash
# 新規発行 (raw token が一度だけ stdout に出る)
sakurasato-server token issue
# 失効
sakurasato-server token revoke <id>
# 一覧
sakurasato-server token list
```

### 1.3 CLI フラグ一覧

| フラグ | env | デフォルト | 用途 |
|---|---|---|---|
| `--socket <path>` | `SAKURASATO_SOCKET` | `/run/sakurasato/local.sock` | UDS 接続先 |
| `--api-url <url>` | `SAKURASATO_API_URL` | (未設定) | TCP 接続先。`http://` のみ |
| `--token <value>` | `SAKURASATO_TOKEN` | (未設定) | Bearer raw token |
| `--token-file <path>` | — | (未設定) | Bearer token をファイルから |
| `--theme <name>` | — | `sakura` | 組み込みテーマ名 |
| `--theme-file <path>` | — | (未設定) | 自前テーマ TOML (組み込みより優先) |
| `--list-themes` | — | — | 組み込みテーマ名を列挙して終了 |
| `--page-size <n>` | — | `40` | timeline 1 ページの件数 (1〜80 にクランプ) |
| `--no-images` | — | off | 視覚刺激抑制の **killswitch**。全要素一括 off |
| `--no-avatars` | — | off | アバター画像を抑制 |
| `--no-attachments` | — | off | 添付画像サムネを抑制 |
| `--no-emojis` | — | off | カスタム絵文字を抑制 |
| `--no-previews` | — | off | ファイル picker のローカルプレビューを抑制 |
| `--no-animations` | — | off | アニメ画像を静止画に倒す |
| `--log-file <path>` | `SAKURASATO_LOG_FILE` | (未設定) | tracing 出力先。未指定なら無効 |

ログのフィルタは `SAKURASATO_LOG` → `RUST_LOG` → `warn` の順で fallback。

---

## 2. 画面構成

```
┌───────────────────────────────────────────────────┐
│ status bar  (account / focus / message)           │
├───────────────────────────────────────────────────┤
│                                                   │
│            メインビュー                             │
│      (timeline / profile / follow list)            │
│                                                   │
├───────────────────────────────────────────────────┤
│ compose / reaction / command prompt  (modal)      │
└───────────────────────────────────────────────────┘
```

- **メインビュー** は timeline がデフォルト。`p` (作者の profile) / `:open` (任意の actor) / `:following` / `:followers` で profile や follow list を push できる。
- **モーダル**: compose / reaction / file picker / command prompt / help / 視覚刺激抑制 overlay は一時的に開く / 閉じる。
- 画面下のメッセージ行が **status line**。エラーや成功通知はここに 1 行で出る (`SAKURASATO_LOG_FILE` を指定していなければここが唯一の情報源)。

---

## 3. キーバインド

### 3.1 Timeline (起動直後)

| Key | 動作 |
|---|---|
| `j` / `↓` | 次の note を選択 |
| `k` / `↑` | 前の note を選択 |
| `Space` / `PgDn` | 1 ページ下 |
| `PgUp` | 1 ページ上 |
| `n` | compose を開く (新規 note) |
| `r` | timeline refresh |
| `o` | 古い note を load more |
| `t` | テーマ cycle |
| `A` | アバター変更 (file picker) |
| `H` | ヘッダ画像変更 (file picker) |
| `;` | 添付画像 picker (compose に持ち越し) |
| `e` | 選択中 note にリアクション (絵文字検索モーダルを直起動 → Enter で即送信) |
| `i` | 視覚刺激抑制 overlay を開く |
| `p` | 選択中 note の作者 profile を push |
| `:` | command prompt |
| `?` | help overlay |
| `q` / `Esc` | TUI 終了 |

### 3.2 Compose

| Key | 動作 |
|---|---|
| 文字 | 本文に入力 |
| `Enter` | 改行 |
| `Ctrl-Enter` | 送信 (Kitty / WezTerm / Alacritty 等の Kitty keyboard protocol 対応端末のみ) |
| `F2` | 送信 (どの端末でも効く恒久 alias) |
| `Ctrl-W` | CW (content warning) フィールド toggle |
| `Ctrl-V` | 可視性 cycle ── `public` → `unlisted` → `followers` → `direct` → `public` |
| `Ctrl-S` | sensitive フラグ toggle |
| `Ctrl-A` | 添付画像 picker (compose 内から) |
| `Ctrl-D` | 直近の添付を 1 件外す |
| `Esc` | compose を離脱 |

添付は最大 **4 件**。

> **送信キーの端末互換**: `Ctrl-Enter` は端末が Kitty keyboard protocol (CSI u) を喋れる必要があります (Kitty / WezTerm / Alacritty / foot 等)。xterm / GNOME Terminal / Konsole / tmux 経由など普通の VT 端末では区別できず改行扱いになるので、**`F2`** を使うのが最も確実です。

### 3.3 File picker (アイコン / ヘッダ / 添付の選択時)

| Key | 動作 |
|---|---|
| `j` / `k` | 次/前 |
| `Enter` | ディレクトリなら降りる / ファイルなら選択 |
| `Backspace` | 親ディレクトリへ |
| `.` | hidden file 表示 toggle |
| `Esc` / `q` | キャンセル |

### 3.4 絵文字検索モーダル

Timeline で `e` を押すと「選択中 Note への即時リアクション」モードで、
Compose で `Ctrl-E` を押すと「本文に `:shortcode:` / Unicode 1 字を挿入」
モードで起動します。同じ overlay を 2 つのモードで使い回す構成です。

| Key | 動作 |
|---|---|
| 文字 | 検索 buffer に入力 (部分一致 + 前方一致優先) |
| `Backspace` | 検索 buffer から 1 文字削除 |
| `↑` / `↓` (or `Ctrl-P` / `Ctrl-N`) | 候補移動 |
| `Enter` | モードに応じて確定 (即リアクション送信 / 本文に挿入) |
| `Esc` | 何もせず閉じる |

候補は 2 ソースを merge:

- **カスタム絵文字** (`:foo:`) ── サーバの emoji DB から取得 (`/api/v1/emojis`)。
  選択中の絵文字は modal のプレビュー枠に画像表示 (`ratatui-image`)。
- **Unicode 絵文字** (`:grinning:` → `😀`、`:+1:` → `👍` 等) ── TUI に焼き込み
  済みの shortcode テーブル ([github/gemoji](https://github.com/github/gemoji)
  由来、MIT)。プレビュー枠には codepoint を中央に表示。

候補 0 件でも閉じません (= 検索 buffer を消せば全候補が戻ります)。

### 3.5 Profile 画面

`p` (timeline で作者を開く) または `:open <acct-or-url>` / `:lookup <acct-or-url>` / `:me` で push。

| Key | 動作 |
|---|---|
| `j` / `k` | 次/前の note |
| `f` | follow / unfollow toggle |
| `o` | 古い note を load more |
| `r` | 関係 (following 状態) + note を再取得 |
| `Esc` / `q` | 直前の画面に戻る |

### 3.6 Follow list 画面

`:following` / `:followers` で push。

| Key | 動作 |
|---|---|
| `j` / `k` | 次/前のエントリ |
| `t` | following / followers タブ切替 |
| `Enter` | 選択中のアカウントの profile を開く |
| `o` | load more |
| `r` | 現在のタブを再取得 |
| `Esc` / `q` | timeline に戻る |

### 3.7 視覚刺激抑制 overlay (`i` で起動)

| Key | 動作 |
|---|---|
| `j` / `k` | 行カーソル移動 |
| `Space` / `Enter` | 選択中の要素を toggle |
| `!` | 全要素 off |
| `Esc` / `i` | 閉じる |

要素は 5 種:

| 要素 | 範囲 | CLI 同等 |
|---|---|---|
| avatar | timeline / profile / follow list の発信者アイコン | `--no-avatars` |
| attachment | note 添付画像のサムネ | `--no-attachments` |
| emoji | カスタム絵文字 | `--no-emojis` |
| preview | ファイル picker のローカルプレビュー | `--no-previews` |
| animation | アニメ画像 | `--no-animations` |

### 3.8 Help overlay (`?` で起動)

`?` 再押下で閉じる。なお現状 18 行に clamp されておりスクロールできず、後半のセクション (command mode 等) が画面外で見えない既知の問題があります ([#90](https://github.com/nananek/sakurasato/issues/90))。本ドキュメントが現状最も網羅的な keymap 一覧です。

### 3.9 Follow requests 画面 (`:requests` で起動)

鍵アカ運用 (`actor.manually_approves_followers = true`、`:lock` で切替) のとき、新規 Follow は auto-Accept されず `pending` のまま溜まる ── それを一覧して個別に承認 / 拒否する画面。

| Key | 動作 |
|---|---|
| `j` / `k` (or `↑` / `↓`) | カーソル移動 |
| `a` | 選択行を **approve** (Accept 配送 + state 遷移) |
| `x` | 選択行を **reject** (Reject 配送 + state 遷移) |
| `r` | 一覧を再取得 |
| `Esc` / `q` | 画面を閉じる (Timeline に戻る) |

各行は `[<follow_id>] <follower_ap_id>  <received_at>` の形で 1 行表示。approve / reject 成功時はその行が即座にリストから消えます。

**注意 1**: `:unlock` しても既に pending の Follow が auto-accept されることはありません ([CLAUDE.md §5.1](../CLAUDE.md))。明示的に `a` で approve する必要があります (= Mastodon と同じ作法、lock 解除事故で全 pending を取り込む暴発を防ぐ)。

**注意 2**: 既存の `accepted` フォロワーが Mastodon 側で Follow を retry してきたケース (`row.state = accepted` で着信) は lock 後でも引き続き Accept が自動で返ります。`:lock` した瞬間に従来フォロワーを切るのではなく、**新規 Follow だけ承認制に切替える** 設計です。

---

## 4. Command mode (`:`)

Timeline focus で `:` を押すと 1 行入力プロンプトが開く。`Enter` で実行、`Esc` でキャンセル。

### 4.1 コマンド一覧

| Command | 動作 |
|---|---|
| `:follow <X>` | follow を投入 (`POST /api/v1/follow`) |
| `:unfollow <X>` | follow 取消 |
| `:open <X>` | profile 画面を push (Mastodon Lookup 相当) |
| `:lookup <X>` | `:open` のエイリアス (Misskey「照会」) |
| `:me` | 自分の profile |
| `:following` | following リスト |
| `:followers` | followers リスト |
| `:lock` | 鍵アカ運用に切替 (`manually_approves_followers = true`、actor `Update` 配送) |
| `:unlock` | 鍵アカ解除 (pending follow は auto-accept されない) |
| `:requests` | 承認待ち follow 一覧画面を開く (= [§3.9](#39-follow-requests-鍵アカ承認画面-で起動)) |
| `:help` / `:?` | help overlay |
| `:q` / `:quit` | TUI 終了 |

### 4.2 `<X>` (acct / URL) の受理形式

| 形式 | 例 | メモ |
|---|---|---|
| acct | `@alice@misskey.io` / `alice@misskey.io` | 先頭 `@` は任意 |
| Mastodon/Misskey permalink | `https://misskey.io/@alice` | host の `@user` を acct に変換 |
| federated permalink | `https://aggregator.example/@alice@home.example` | `@user@otherhost` から `user@otherhost` を抽出 |
| AP URI | `https://misskey.io/users/abcd1234` | そのまま server に渡す (`?ap_id=` 経路) |

`http://` も受理 (= 開発環境互換)。本番では SSRF 対策が server 側 `net_guard` に効きます。

---

## 5. テーマ

組み込み:

```bash
sakurasato-tui --list-themes
```

切替は **起動時** に `--theme <name>` で指定するか、**起動後** に timeline focus で `t` を押すと cycle する。

自前テーマは `config/themes/*.toml` を参考に書き、`--theme-file <path>` で読み込む。

---

## 6. ログ

未指定時は subscriber 自体を初期化しないため何も出ない (端末を独占している間に warn/error が描画と混ざる事故を防ぐため)。デバッグしたいときだけファイル指定:

```bash
# 端末 A
sakurasato-tui --log-file /tmp/sakurasato-tui.log

# 端末 B (別ウィンドウ)
tail -f /tmp/sakurasato-tui.log
```

env でも同じ:

```bash
SAKURASATO_LOG_FILE=/tmp/sakurasato-tui.log sakurasato-tui
```

フィルタは `SAKURASATO_LOG` → `RUST_LOG` → `warn` の順 (env で `info` / `debug` / `trace` 等が指定可)。

---

## 7. トラブルシューティング

### 7.1 画像が表示されない

1. Kitty graphics / Sixel / iTerm2 inline image protocol いずれかに対応した端末で起動しているか確認。`ratatui-image` の自動検出で全て非対応ならテキスト UI に倒れる。
2. `--no-images` を指定していないか。
3. `i` overlay の各要素が ON になっているか (= 個別 off で隠している可能性)。

### 7.2 warn / error が画面に流れる

`--log-file` を指定してから再起動。未指定時は subscriber が初期化されないため流れません。([fix in #91](https://github.com/nananek/sakurasato/pull/91))

### 7.3 server に繋がらない

- **UDS の場合**: socket ファイルが存在するか + 読み書き権限があるか確認。docker rootless で compose 内に閉じている場合はホストから触れないので、`--api-url tcp://...` 経路 (Tailscale や直 TCP) に倒す ([DEPLOYMENT.md §5](../DEPLOYMENT.md))。
- **TCP の場合**: `--api-url` は `http://host:port` 形式必須。`https://` は明示拒否される。
- **トークン**: `401 Unauthorized` なら `sakurasato-server token list` で発行済み token を確認、必要なら `revoke` → `issue` で再発行 (raw 値は **1 度きり** stdout 表示)。

### 7.4 フォローしたのに timeline に何も出ない

Home timeline は **`follow.state = 'accepted'` の followee の投稿 + 自分の投稿** だけを出します (`direct` 可視性は除外、実装は [`crates/core/src/repo/note.rs`](../crates/core/src/repo/note.rs) の `list_home_timeline`)。「見えない」の切り分け順:

1. **follow が `accepted` に到達しているか確認**: `:open @相手@host` で profile を開き、follow ボタンの状態を見る。`pending` なら相手が鍵アカで承認待ち、もしくは Accept 配送が詰まっている。
2. **相手から Create が届いているか**: Mastodon / Misskey はバックフィルしないので、**Accept 完了より前の投稿は流れてこない**。相手が新規に投稿してから 1〜2 分待つ。
3. **inbox 取り込みが失敗していないか**: server (TUI ではなく) のログで `Create` / `inbox` 周辺の error を確認。
4. **visibility=direct で除外されていないか**: 相手が DM しか送っていないと timeline には出ない仕様。
5. **TUI のカーソルが古いまま**: timeline 上で `r` を押して refresh。SSE 接続が切れていると initial fetch のままになる。

最速の切り分け: `:open` で相手の profile を開いて、

- 相手の投稿が並んでいる → DB には入っている → timeline 取り込み側の問題
- profile も空 → 配送が来ていない (上の 2 か 3)
- ボタンが `follow` のまま → 1 (まだ accepted まで到達していない)

### 7.5 Kitty 端末で画像がチラつく / 残骸が残る

- 視覚刺激抑制 overlay (`i`) で animation を off にする / `--no-animations` で起動。
- それでも残るときは `--no-images` で killswitch。

---

## 関連ドキュメント

- [DEPLOYMENT.md](../DEPLOYMENT.md) ── サーバの本番運用 / Cloudflare Tunnel / Tailscale 経由の TUI
- [CLAUDE.md](../CLAUDE.md) ── アーキテクチャ全体像と設計方針
- [README.md](../README.md) ── プロジェクト概要
