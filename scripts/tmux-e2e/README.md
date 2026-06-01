# tmux-e2e ── sakurasato-tui を実 pty で駆動する harness

Issue #119 / 親 #58 (M12 PR1)。

このディレクトリは sakurasato-tui を tmux 内で実 pty 駆動するための
**driver 単体** を提供する。compose / Nekonoverse 連携 / 実シナリオ / CI
workflow は後続 PR (#NEW2) で組み合わせる。

## なぜ tmux か (旧 `--scripted` 案を採らなかった理由)

1. **TUI バイナリに二系統を持たせない**: 対話 TUI と `--scripted` stdin/stdout
   駆動の二系統は、コード重複・分岐の維持コストが高い。
2. **キー入力経路をテスト対象に含める**: `#118` のような「Kitty keyboard
   protocol の絡みで `Enter` が届かない」型バグは stdin パイプ駆動では絶対に
   検出できない。tmux は実 pty を提供するので、enhancement 検出を含む経路で
   実発火する。
3. **画面表示も assertion 対象**: `capture-pane` で TL レンダリング・status
   bar・絵文字描画 (テキスト部分) の回帰を検出できる。

## 構成

| ファイル | 役割 |
|---|---|
| [`lib.sh`](lib.sh) | bash shell ライブラリ。`tmux_start` / `tmux_send_keys` / `tmux_capture` / `wait_until_text` / `tmux_kill` |
| [`conftest.py`](conftest.py) | pytest fixture (`tmux_session` / `tmux_tui`)。`lib.sh` を `subprocess.run` で薄くラップ |
| [`test_driver.py`](test_driver.py) | 単体テスト 5 件。`cat` / `sh -c echo` で driver の挙動を検証 |

### tmux server 隔離

`lib.sh` 内の tmux 呼び出しはすべて `-L sks-e2e` (env で
`TMUX_E2E_SOCKET` で上書き可) を付けて専用 socket 上で動く。開発機で既に
動いている個人 tmux session を巻き込まない / 巻き込まれない。

## 使い方

### bash から直接

```bash
set -euo pipefail
source scripts/tmux-e2e/lib.sh

sess="$(tmux_unique_session_name scenario)"
trap 'tmux_kill "$sess"' EXIT

tmux_start "$sess" cat
tmux_send_keys "$sess" "hello" Enter
wait_until_text "$sess" "hello" 5
tmux_capture "$sess"
```

### pytest から (PR2 以降の実シナリオ)

```python
def test_sks_follows_nkv_and_sees_note(tmux_tui, nkv_client):
    tui = tmux_tui("/sock/path", "token-xxx")
    tui.send_keys(":follow @bob@nkv.test", "Enter")
    tui.wait_until_text(r"follow .* accepted", 10)

    nkv_client.post_note("hello from bob")
    tui.wait_until_text("hello from bob", 30)
```

### 任意プロセスを駆動する (= 本 PR の単体テスト)

```python
def test_xxx(tmux_session):
    s = tmux_session("sh", "-c", "echo READY; exec sleep 60")
    s.wait_until_text("READY", 5)
    assert "READY" in s.capture()
```

## 「固定 sleep 禁止」ポリシー

CI flakiness の主因なので、**`sleep N` (= 数値固定の wait) を本ディレクトリで
書かない**。条件待ちは必ず `wait_until_text` を経由する。

例外は polling 用 `sleep "$TMUX_E2E_POLL_INTERVAL"` (既定 0.2s) 1 箇所だけ ──
これは `wait_until_text` 関数内に閉じ込めて、外向きには露出しない。

### lint

```bash
# 行末 `sleep N` (= 単独コマンドとしての固定 sleep) を検出。
# `sh -c "... exec sleep 60"` 等の test fixture 中の sleep は対象外。
grep -rnE 'sleep [0-9]+$' scripts/tmux-e2e/
```

このコマンドが何も出力しないこと。polling 用 `sleep "$TMUX_E2E_POLL_INTERVAL"`
は数値ではなく env 経由の変数なので `$` anchor の検査では引っかからない
(= 関数内に閉じ込めた polling は許容、それ以外の固定 sleep は禁止、という
ポリシーがそのまま grep で表現できる)。

## ローカル実行

```bash
cd scripts/tmux-e2e
pip install pytest  # 既にあれば不要
pytest -v test_driver.py
```

期待: 5 件 pass。tmux が PATH に無い環境では全件 skip される。

## 設計判断のメモ

### pane サイズ

`tmux_start` は `-x 200 -y 50` を明示。tmux の既定はクライアントの解像度に
合わせるが、`new-session -d` (= detached) ではクライアントが居ないので 0x0
近辺になり、ratatui のレンダリングが壊れることがある。Kitty の標準寸法に
近い値で固定する。

### capture-pane の `-J` を付けない

`-J` は折り返し行を「改行なしで連結」する。これは grep しづらいので、既定の
「行ごとに改行」モードで出す (= `-J` なし)。

### Python 側で再実装しない理由

bash ロジックを Python に書き直すと、`scripts/` を bash 単体で叩く CI 経路と
pytest 経路で挙動がズレる。`subprocess.run` で `lib.sh` を毎回 source して呼ぶ
オーバーヘッドは E2E のオーダ (秒単位) に対して無視できる。

### session 名の衝突回避

`{prefix}-{pid}-{rand}` で生成する (bash 側 `tmux_unique_session_name` /
Python 側 `_unique_session_name`)。並列 pytest 実行 (`pytest-xdist`) でも
session 名は worker pid で区別されるので衝突しない。

## 関連

- 親: #58 (E2E 連合テスト umbrella)
- 後続: #NEW2 = compose + Nekonoverse + 実シナリオ + `.github/workflows/`
- 関連バグ例: #118 (この harness で検出可能な「キー入力が届かない」型)
