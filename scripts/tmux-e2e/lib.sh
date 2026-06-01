#!/usr/bin/env bash
# tmux 内で sakurasato-tui (または任意の TUI バイナリ) を実 pty 駆動するための
# 最小ライブラリ。
#
# Issue #119 / #58 (M12 PR1):
#   - TUI バイナリに stdin/stdout 駆動 (= 旧 `--scripted` 案) を入れず、実際の
#     pty 越しに `send-keys` / `capture-pane` で操作する。
#   - 固定 sleep を撲滅する。条件待ちは `wait_until_text` を通す。
#
# 使用方法:
#   source "$(dirname "$0")/lib.sh"
#   sess="$(tmux_unique_session_name myscenario)"
#   tmux_start "$sess" cat               # 任意のバイナリ
#   tmux_send_keys "$sess" "hello" Enter
#   wait_until_text "$sess" "hello" 5
#   tmux_kill "$sess"
#
# 本ファイルは bash で書かれている (= `set -euo pipefail` を前提に呼び出し側を
# 縛る). sh では動かない。

# ─────────────────────────────────────────────────────────────────────
# 定数
# ─────────────────────────────────────────────────────────────────────

# capture-pane を回す間隔。テストの時間予算とは独立した実装詳細なので
# 短くしすぎず (CPU 焼け回避) / 長くしすぎず (体感遅延回避) 0.2s に固定。
# 唯一許容される固定 sleep ── 関数内に閉じ込めて外には露出しない。
TMUX_E2E_POLL_INTERVAL="${TMUX_E2E_POLL_INTERVAL:-0.2}"

# tmux サーバを共有しないための専用 socket 名。CI / 開発機の他の tmux session
# と分離する (= 既存 user の tmux に session が混ざらない) ために `-L` で
# 名前付き socket を指定する。socket name 単位で server が分かれる。
# Issue #119: 個別ホスト上で別の tmux server を巻き込まないため。
TMUX_E2E_SOCKET="${TMUX_E2E_SOCKET:-sks-e2e}"

# tmux 共通フラグ。本ファイル内のすべての tmux 呼び出しはこの prefix を通す。
_tmux() {
    tmux -L "$TMUX_E2E_SOCKET" "$@"
}

# ─────────────────────────────────────────────────────────────────────
# 公開 API
# ─────────────────────────────────────────────────────────────────────

# session 名を生成する。pid + random suffix で衝突回避。
# Usage: name="$(tmux_unique_session_name <prefix>)"
tmux_unique_session_name() {
    local prefix="${1:-sks-e2e}"
    # `$RANDOM` は bash builtin (16-bit)。pid と組み合わせれば衝突確率は十分低い。
    printf 'sks-e2e-%s-%s-%s\n' "$prefix" "$$" "$RANDOM"
}

# 任意コマンドを新規 tmux session で detached 起動する。
# Usage: tmux_start <session> <cmd> [args...]
#
# 既に同名 session があれば失敗する (= テスト側で名前を unique にすること)。
# 起動コマンドの引数は exec form で渡される (= shell 解釈を受けない)。
tmux_start() {
    if [[ $# -lt 2 ]]; then
        echo "tmux_start: usage: tmux_start <session> <cmd> [args...]" >&2
        return 64
    fi
    local session="$1"
    shift
    if _tmux has-session -t "$session" 2>/dev/null; then
        echo "tmux_start: session '$session' already exists" >&2
        return 1
    fi
    # `-d` で detached。`-x` / `-y` で pane サイズを明示 ── 既定だと
    # クライアント解像度に合わせて 0x0 になり、ratatui レンダリングが崩れる
    # ことがあるため。Kitty の標準寸法に近い 200x50 を取る。
    # `-s` session 名、`--` の後がコマンド + args。
    _tmux new-session -d -s "$session" -x 200 -y 50 -- "$@"
}

# sakurasato-tui を新規 tmux session で起動する thin wrapper。
# Usage: tmux_start_tui <session> <socket_path> <token> [args...]
#
# SAKURASATO_TUI_BIN env var でバイナリ path を上書きできる
# (= テスト stub / cargo build 済み path / 本番 path を切替)。
tmux_start_tui() {
    if [[ $# -lt 3 ]]; then
        echo "tmux_start_tui: usage: tmux_start_tui <session> <socket> <token> [args...]" >&2
        return 64
    fi
    local session="$1" socket="$2" token="$3"
    shift 3
    local bin="${SAKURASATO_TUI_BIN:-sakurasato-tui}"
    # TUI バイナリの env で socket / token を渡す。CLI 引数の形は
    # crates/tui の clap で確定したら追従させる ── 現状の dryrun では
    # env だけで成立しているはず。
    SAKURASATO_LOCAL_API_SOCKET="$socket" \
    SAKURASATO_LOCAL_API_TOKEN="$token" \
        tmux_start "$session" "$bin" "$@"
}

# send-keys ラッパ。可変長引数をそのまま tmux に渡す。
# Usage: tmux_send_keys <session> <keys> [<keys>...]
#
# tmux send-keys は各引数を「文字列リテラル」または「特殊キー名」と解釈する:
#   tmux_send_keys "$s" "hello" Enter       # "hello" → Enter
#   tmux_send_keys "$s" C-c                  # Ctrl-C
#   tmux_send_keys "$s" ":follow @bob" Enter
#
# 改行 (= Enter キー押下) は必ず `Enter` literal で渡す。`"\n"` を含めないこと。
tmux_send_keys() {
    if [[ $# -lt 2 ]]; then
        echo "tmux_send_keys: usage: tmux_send_keys <session> <keys> [<keys>...]" >&2
        return 64
    fi
    local session="$1"
    shift
    _tmux send-keys -t "$session" "$@"
}

# capture-pane の thin wrapper。現画面を stdout に書き出す。
# Usage: tmux_capture <session>
#
# `-p` でファイル出力ではなく stdout、`-J` は **付けない** (= 折り返し行は
# 改行で区切る方が grep しやすい)。
tmux_capture() {
    if [[ $# -ne 1 ]]; then
        echo "tmux_capture: usage: tmux_capture <session>" >&2
        return 64
    fi
    _tmux capture-pane -p -t "$1"
}

# capture-pane を polling して regex が現画面に現れるのを待つ。
# Usage: wait_until_text <session> <regex> <timeout_sec>
#
# - timeout したら最終画面を stderr にダンプして 1 を返す。
# - regex は grep -E (ERE) として解釈される。
wait_until_text() {
    if [[ $# -ne 3 ]]; then
        echo "wait_until_text: usage: wait_until_text <session> <regex> <timeout_sec>" >&2
        return 64
    fi
    local session="$1" regex="$2" timeout="$3"
    local start
    start="$(date +%s)"
    while :; do
        if tmux_capture "$session" 2>/dev/null | grep -Eq -- "$regex"; then
            return 0
        fi
        local now
        now="$(date +%s)"
        if (( now - start >= timeout )); then
            {
                echo "wait_until_text: timeout after ${timeout}s waiting for /$regex/ in '$session'"
                echo "--- capture-pane ---"
                tmux_capture "$session" 2>/dev/null || echo "(capture failed: session gone?)"
                echo "--- end capture ---"
            } >&2
            return 1
        fi
        sleep "$TMUX_E2E_POLL_INTERVAL"
    done
}

# session を kill する。存在しなくても成功扱い (idempotent)。
# Usage: tmux_kill <session>
tmux_kill() {
    if [[ $# -ne 1 ]]; then
        echo "tmux_kill: usage: tmux_kill <session>" >&2
        return 64
    fi
    local session="$1"
    if _tmux has-session -t "$session" 2>/dev/null; then
        _tmux kill-session -t "$session"
    fi
}

# tmux server (= 本ライブラリ専用 socket) を落とす。複数 session を一括で
# 片付ける時用。テストの session 単位 cleanup は tmux_kill で十分。
tmux_e2e_kill_server() {
    if _tmux ls 2>/dev/null >/dev/null; then
        _tmux kill-server
    fi
}
