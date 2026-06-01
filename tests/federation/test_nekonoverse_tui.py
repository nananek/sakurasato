"""Sakurasato TUI × Nekonoverse smoke (#58 / #120 PR2a).

シナリオ 1: ``follow_post`` (= 本 PR で扱うのは Follow + Accept まで)

  1. sakurasato-tui を tmux pty で起動し、timeline が描画されるまで待つ。
  2. ``:`` で command prompt を開き、``:follow bob@nekonoverse`` を入力。
  3. 連合経由で Nekonoverse 側から ``Accept`` が戻ってくるのを、
     **sakurasato 側の local API** (`SakurasatoClient`) で確認する
     (= ``/api/v1/whoami`` / フォロー状態を覗ける public 経路は無いため、
     UDS 側で詰めるのが現状の正解)。
  4. Nekonoverse 側でも bob の followers リストに sakurasato:me が
     現れるのを ``NekonoverseClient`` (httpx で Mastodon 互換 public API
     ``/accounts/lookup`` + ``/accounts/{id}/followers`` を叩く) で確認。

シナリオ外 (= PR2b 以降):

- nkv 側で bob が note 投稿 → sks TL に出現
- カスタム / Unicode emoji リアクション (#118 回帰テスト)
- Reply / Move / Actor Update

実行前提:

- ``compose/docker-compose.federation-nekonoverse.yml`` の ``pytest`` profile
  が起動済み (= ``sakurasato_local_api`` 共有 volume が pytest コンテナに見える)。
- TUI バイナリは Dockerfile.tmux で ``/usr/local/bin/sakurasato-tui`` に
  焼かれている。
- ``SAKURASATO_TOKEN_FILE`` は ``sakurasato-token-issuer`` 1-shot コンテナが
  書いた状態。Nekonoverse 側 token は **PR2a スコープ外** ── public な
  ``/api/v1/accounts/lookup`` + ``/api/v1/accounts/{id}/followers`` だけで
  Follow + Accept round-trip を verify する。

flakiness 対策:

- 連合配送は worker 経由なので時間揺れがある (Nekonoverse / sakurasato 双方の
  キュー)。``poll_until`` で長め (90s) に待つ。
- tmux capture は polling ベース。`wait_until_text` は ``lib.sh`` の
  ``TMUX_E2E_POLL_INTERVAL`` (default 0.2s) で更新する。
"""
from __future__ import annotations

import pytest

from conftest import (
    NEKONOVERSE_DOMAIN,
    SAKURASATO_DOMAIN,
    NekonoverseClient,
    SakurasatoClient,
    poll_until,
)


# シナリオが触る相手 acct。compose 側 fixture と整合。
BOB_LOCAL = "bob"
BOB_ACCT = f"{BOB_LOCAL}@{NEKONOVERSE_DOMAIN}"
SKS_ACCT = f"me@{SAKURASATO_DOMAIN}"


def _send_follow_command(tui, acct: str) -> None:
    """TUI に ``:follow <acct>`` を入力して Enter を打つ。

    `:` でコマンドプロンプトを開き、続けて引数を打って Enter。
    `:` 自体は ``send_keys`` の引数 1 つに入れる (= shell の特殊解釈は
    `lib.sh::tmux_send_keys` 側で吸収済み)。
    """
    tui.send_keys(":", f"follow {acct}", "Enter")


@pytest.mark.timeout(300)
def test_follow_bob_round_trips_to_accept(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """Timeline → ``:follow @bob@nekonoverse`` → Accept until both sides agree."""

    # 1. TUI 起動。`--no-images` で Kitty/Sixel 検出を抑止 (= headless CI)。
    tui = tmux_tui(
        sakurasato_socket_path,
        sakurasato_token_file,
        "--no-images",
        label="follow_bob",
    )

    # 2. Timeline が出るまで待つ。空 TL でも "@me" の status バー or "timeline"
    #    の focus ラベルは必ず出る。前者の方が誤検出しにくい。
    tui.wait_until_text(r"@me", 30)

    # 3. command prompt を経由して follow を送る。
    _send_follow_command(tui, BOB_ACCT)

    # 4. TUI status line で「follow が投入された」ことを確認する。
    #    runtime::command_follow_target は成功時に `"follow requested → @bob@..."` を
    #    出す (= 投入は OK)。Accept が戻るまで待つのは後段で `following()` に任せる。
    tui.wait_until_text(r"follow requested", 30)

    # 5. sakurasato 側 `/api/v1/following` を polling して、bob@nekonoverse が
    #    accepted 一覧に出るまで待つ。連合配送 (sks → nkv POST Follow) + nkv
    #    側 Accept 配送 + sks 受領処理 + state 遷移 (`pending` → `accepted`) の
    #    全フェーズを抜けるので 90s 程度の余裕を見る。
    def follow_accepted() -> bool:
        try:
            for f in sakurasato.following(limit=80):
                # entry 形: `FollowWithActor` (= `{actor: {host, preferred_username, ...}, ...}`)
                # M13 PR3 の `repo::follow::list_following` 形に合わせる。
                actor = f.get("actor") or {}
                host = (actor.get("host") or "").lower()
                name = (actor.get("preferred_username") or "").lower()
                if name == BOB_LOCAL and host == NEKONOVERSE_DOMAIN.lower():
                    return True
        except Exception:
            return False
        return False

    poll_until(follow_accepted, timeout=90, interval=2, desc="sks following bob")

    # 6. Nekonoverse 側で bob の followers に sakurasato:me が並ぶことを確認。
    #    連合配送 (= sks → nkv POST /inbox の Follow) が成立した直接の証拠。
    #    PR2a スコープでは Bearer を持たないので、Mastodon 互換の public 経路
    #    (`/accounts/lookup` + `/accounts/{id}/followers`) で取り回す。
    bob_account = nekonoverse.lookup_account(BOB_LOCAL)
    bob_account_id = bob_account["id"]

    def me_in_bob_followers() -> bool:
        try:
            followers = nekonoverse.followers(bob_account_id)
        except Exception:
            return False
        return any(
            (f.get("acct") or "").lower() == SKS_ACCT.lower()
            or (f.get("username") or "").lower() == "me"
            for f in followers
        )

    poll_until(
        me_in_bob_followers,
        timeout=90,
        interval=3,
        desc="me@sakurasato in bob followers (nkv side)",
    )

    # 7. cleanup ── TUI は teardown で kill される。明示的に quit させると
    #    ratatui が alt-screen を残さず exit するので status bar を綺麗に
    #    片付けられる。失敗してもテストの verdict には影響させない。
    try:
        tui.send_keys(":", "quit", "Enter")
        tui.wait_until_text(r"\$", 5)
    except Exception:  # noqa: BLE001
        pass
