"""pytest fixtures for the tmux-driven E2E harness.

`lib.sh` 側に書かれた bash 関数を `subprocess.run()` で呼ぶ薄いラッパ。
ロジックを Python 側に再実装しないことで、CI shell スクリプトと pytest が
同じ挙動になるようにしている。

主な公開 fixture:

- `tmux_session`: 任意コマンドを tmux 内で起動するファクトリ。
  ::

      def test_xxx(tmux_session):
          s = tmux_session("cat")
          s.send_keys("hello", "Enter")
          s.wait_until_text("hello", 5)

- `tmux_tui`: sakurasato-tui を tmux 内で起動するファクトリ。socket / token
  を引数で受ける (PR2 で実 TUI シナリオから使う想定)。
  ::

      def test_yyy(tmux_tui):
          tui = tmux_tui(socket_path, token)
          tui.send_keys(":quit", "Enter")

両 fixture とも function スコープで、テスト終了時に session を自動 kill する。
"""
from __future__ import annotations

import os
import random
import shlex
import subprocess
import time
from pathlib import Path
from typing import Iterator

import pytest

# `lib.sh` までの絶対 path。conftest.py と同じディレクトリに置く前提。
LIB_SH = Path(__file__).resolve().parent / "lib.sh"

# テスト session 名の prefix。Python 側で生成して `tmux_start` に直接渡す。
# (lib.sh の `tmux_unique_session_name` を呼ぶよりも、Python 側で生成した方が
#  cleanup 時に session 名が確定して扱いやすい)
_SESSION_PREFIX = "sks-e2e-pytest"


def _run_lib(fn: str, *args: str, check: bool = True, capture: bool = True,
             timeout: float | None = 60.0) -> subprocess.CompletedProcess:
    """`lib.sh` の関数 `fn` を `args` で呼ぶ。

    `bash -c 'source LIB; FN ARGS...'` 形式を使う ── lib.sh は bash 専用なので
    /bin/sh は不可。`set -euo pipefail` を明示してエラーを subprocess 経由で
    検出可能にする。
    """
    # `"$1" "${@:2}"`: $1 を関数名として実行し、$2 以降を引数で渡す。
    # 文字列展開ではなく positional parameter 経由なので、`args` に空白や
    # 特殊文字を含んでも安全。
    script = 'set -euo pipefail; source "$1"; shift; "$1" "${@:2}"'
    cmd = ["bash", "-c", script, "_", str(LIB_SH), fn, *args]
    return subprocess.run(
        cmd,
        check=check,
        capture_output=capture,
        text=True,
        timeout=timeout,
    )


class TmuxSession:
    """1 つの tmux session を表す Python ラッパ。

    インスタンスは fixture からのみ生成すること (= cleanup を fixture に任せる
    前提)。`kill()` は冪等。
    """

    def __init__(self, session_name: str) -> None:
        self.name = session_name
        self._killed = False

    # ── primitives ────────────────────────────────────────────
    def send_keys(self, *keys: str) -> None:
        """tmux send-keys に可変長引数を渡す。

        各引数は tmux send-keys の 1 つの「キー / 文字列」として解釈される。
        改行は文字列に含めず、`"Enter"` を別引数として渡すこと。
        ::

            s.send_keys("hello", "Enter")
            s.send_keys(":follow @bob@nkv.test", "Enter")
            s.send_keys("C-c")   # Ctrl-C
        """
        if not keys:
            raise ValueError("send_keys requires at least one key argument")
        _run_lib("tmux_send_keys", self.name, *keys)

    def capture(self) -> str:
        """`capture-pane -p` を返す (末尾改行は保持)。"""
        res = _run_lib("tmux_capture", self.name)
        return res.stdout

    def wait_until_text(self, regex: str, timeout_sec: float) -> None:
        """画面に regex が出るまで待つ。失敗時は AssertionError。

        実装は lib.sh の `wait_until_text` に委譲。stderr に最終画面が
        ダンプされ、AssertionError に同 stderr 全文を付加する。
        """
        # lib.sh 側は整数秒を受けるので、丸めて渡す。0 秒未満は拒否。
        if timeout_sec < 0:
            raise ValueError("timeout_sec must be >= 0")
        # `check=False`: timeout は subprocess の rc=1 で表現される。
        res = _run_lib(
            "wait_until_text",
            self.name,
            regex,
            str(int(timeout_sec)),
            check=False,
            # Python 側 timeout は lib.sh 側 + 余裕分。lib.sh のループが
            # 暴走したケースを catch するセーフティネット。
            timeout=float(timeout_sec) + 30.0,
        )
        if res.returncode == 0:
            return
        raise AssertionError(
            f"wait_until_text(/{regex}/, {timeout_sec}s) timed out\n"
            f"--- lib.sh stderr ---\n{res.stderr}"
        )

    def kill(self) -> None:
        """session を破棄する。冪等。"""
        if self._killed:
            return
        # 失敗してもテスト本体の verdict を変えたくないので check=False。
        _run_lib("tmux_kill", self.name, check=False)
        self._killed = True


def _unique_session_name(label: str) -> str:
    # bash 側 `tmux_unique_session_name` と等価。pid + random で衝突回避。
    return f"{_SESSION_PREFIX}-{label}-{os.getpid()}-{random.randint(0, 1 << 24)}"


# ─────────────────────────────────────────────────────────────
# fixtures
# ─────────────────────────────────────────────────────────────


@pytest.fixture
def tmux_session() -> Iterator:
    """任意コマンドを tmux 内で起動するファクトリ fixture。

    Usage::

        def test_xxx(tmux_session):
            s = tmux_session("cat")
            s.send_keys("hello", "Enter")
            s.wait_until_text("hello", 5)

    複数 session を 1 テスト内で起動した場合も、teardown で全部 kill する。
    """
    started: list[TmuxSession] = []

    def factory(*cmd: str, label: str = "session") -> TmuxSession:
        if not cmd:
            raise ValueError("tmux_session factory requires a command")
        name = _unique_session_name(label)
        _run_lib("tmux_start", name, *cmd)
        session = TmuxSession(name)
        started.append(session)
        return session

    try:
        yield factory
    finally:
        for s in started:
            s.kill()


@pytest.fixture
def tmux_tui(tmux_session) -> Iterator:
    """sakurasato-tui を tmux 内で起動するファクトリ fixture。

    PR2 以降の実シナリオで使う想定。PR1 (= 本 PR) では実 TUI 連携が
    無いので、テストでは `tmux_session("cat")` 等を直接使う。

    Usage::

        def test_yyy(tmux_tui):
            tui = tmux_tui(socket_path, token)
            tui.send_keys(":quit", "Enter")

    `SAKURASATO_TUI_BIN` 環境変数でバイナリ path を上書き可能。
    """
    started: list[TmuxSession] = []

    def factory(socket_path: str, token: str, *extra_args: str,
                label: str = "tui") -> TmuxSession:
        bin_path = os.environ.get("SAKURASATO_TUI_BIN", "sakurasato-tui")
        name = _unique_session_name(label)
        # lib.sh の `tmux_start_tui` を経由せず Python 側で env + tmux_start に
        # 展開する ── tmux_start_tui は env 経由で渡す仕組みだが、Python から
        # 環境変数を bash 経由でリレーすると subprocess 境界で消えるため。
        # 代わりに sakurasato-tui を `env KEY=VALUE bin` 形式で起動する。
        cmd = (
            "env",
            f"SAKURASATO_LOCAL_API_SOCKET={socket_path}",
            f"SAKURASATO_LOCAL_API_TOKEN={token}",
            bin_path,
            *extra_args,
        )
        _run_lib("tmux_start", name, *cmd)
        session = TmuxSession(name)
        started.append(session)
        return session

    try:
        yield factory
    finally:
        for s in started:
            s.kill()


# ─────────────────────────────────────────────────────────────
# session-scoped: tmux server cleanup at the very end
# ─────────────────────────────────────────────────────────────


@pytest.fixture(scope="session", autouse=True)
def _tmux_server_cleanup() -> Iterator:
    """テスト session 終了時に lib.sh 専用 tmux server を落とす。

    並行する別 pytest プロセスが居る場合は kill-server が他テストに影響する
    可能性があるが、本ライブラリは専用 socket (`-L sks-e2e`) を使っているので
    他 tmux サーバとは分離されている。CI 上では 1 リポジトリ 1 pytest プロセス
    なので問題ない。
    """
    yield
    _run_lib("tmux_e2e_kill_server", check=False)
