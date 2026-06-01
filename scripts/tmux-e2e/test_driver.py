"""tmux driver harness の単体テスト。

実 sakurasato-tui は使わず、`cat` / `sh -c` 等の単純プロセスを tmux 内で動かし、
`tmux_session` factory の `send_keys` / `capture` / `wait_until_text` / `kill` の
挙動を確認する。

設計方針:

- 固定 sleep を使わない。条件待ちは `wait_until_text` 経由。
- 各テストは独立 session を立てる (= teardown で fixture が kill する)。
"""
from __future__ import annotations

import shutil

import pytest

# tmux が居ない環境では skip (= ローカル開発機で意図せず CI fail を踏まない)。
pytestmark = pytest.mark.skipif(
    shutil.which("tmux") is None,
    reason="tmux binary not found on PATH",
)


def test_capture_initial_output(tmux_session) -> None:
    """起動コマンドが標準出力に書いた文字を capture-pane で読める。"""
    s = tmux_session("sh", "-c", "echo READY-MARKER; exec sleep 60",
                     label="initial-output")
    s.wait_until_text("READY-MARKER", 5)
    # capture が string を返し、READY-MARKER が含まれることを直接 assert。
    captured = s.capture()
    assert "READY-MARKER" in captured


def test_send_keys_with_enter(tmux_session) -> None:
    """`Enter` literal を含む send_keys で、`cat` が行を echo して返す。

    pty 越しに `cat` を動かすと、tmux send-keys で入力した文字列が pty
    エコーで 1 度画面に出て、改行確定後に `cat` が同じ行を再出力する
    (= 画面上に同じ文字が 2 回現れる)。少なくとも 1 回見えれば送信経路は
    成立しているので、`>=1` を assert する。
    """
    s = tmux_session("cat", label="cat-enter")
    s.send_keys("HELLO-PIPE", "Enter")
    s.wait_until_text("HELLO-PIPE", 5)


def test_wait_until_text_timeout_dumps_capture(tmux_session) -> None:
    """マッチしない regex で timeout し、AssertionError に capture が乗る。

    `wait_until_text` は失敗時 lib.sh の stderr (=末尾 capture 全文) を
    AssertionError メッセージに含めるので、CI ログでデバッグできる。
    """
    s = tmux_session("sh", "-c", "echo OTHER-MARKER; exec sleep 60",
                     label="wait-timeout")
    # まず capture に何か出るのを待ってから ── これで失敗時の dump 内容が
    # 確実に non-empty になる。
    s.wait_until_text("OTHER-MARKER", 5)
    with pytest.raises(AssertionError) as excinfo:
        # 出るはずのない regex で 1 秒だけ待つ
        s.wait_until_text("THIS-SHOULD-NEVER-APPEAR", 1)
    msg = str(excinfo.value)
    assert "THIS-SHOULD-NEVER-APPEAR" in msg
    # lib.sh が stderr に capture を吐いた中身が来ている
    assert "OTHER-MARKER" in msg


def test_kill_is_idempotent(tmux_session) -> None:
    """`kill()` を 2 度呼んでも例外を出さない。"""
    s = tmux_session("sh", "-c", "exec sleep 60", label="kill-twice")
    s.kill()
    s.kill()  # 2 回目は no-op


def test_multiple_sessions_are_isolated(tmux_session) -> None:
    """同時に起動した 2 session の capture が混ざらない。"""
    a = tmux_session("sh", "-c", "echo TAG-AAA; exec sleep 60", label="iso-a")
    b = tmux_session("sh", "-c", "echo TAG-BBB; exec sleep 60", label="iso-b")
    a.wait_until_text("TAG-AAA", 5)
    b.wait_until_text("TAG-BBB", 5)
    cap_a = a.capture()
    cap_b = b.capture()
    assert "TAG-AAA" in cap_a
    assert "TAG-BBB" not in cap_a
    assert "TAG-BBB" in cap_b
    assert "TAG-AAA" not in cap_b
