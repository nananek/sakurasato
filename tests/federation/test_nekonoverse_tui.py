"""Sakurasato TUI × Nekonoverse 連合シナリオ (#58 / #120 PR2a + PR2b)。

PR2a (smoke 1 本):

  1. ``test_follow_bob_round_trips_to_accept``
     ── ``:`` で command prompt を開き ``:follow bob@nekonoverse`` → Accept
     round-trip までを確認する (= TUI 経路で follow が確立する)。

PR2b (本 PR で追加):

  2. ``test_bob_note_appears_in_sks_timeline``
     ── bob が nkv 側で note 投稿 → 連合配送で sks home timeline に出現する
     ことを確認 (= 取得側 = sks の inbox / TL の受領パス)。

シナリオ外 (= PR2c 以降 / 他 PR で必要に応じて):

- **sks → bob リアクション** (#118 Enter 回帰テスト) ── PR2b 着手時に
  ``POST /api/v1/reactions`` が remote note に対して 404
  ``"reactions to remote notes are not supported yet"`` を返す **server 側
  ギャップ** が判明したため、本 PR では実装を見送る。TUI 側の Enter 経路
  そのものは tmux 経由で fire することは検証済 (= status line に上記 404 が
  出るところまで送れる)。サーバ側ギャップを別 issue で先に閉じてから
  リアクション連合シナリオを追加する流れに倒した。
- カスタム emoji reaction (`:shortcode:` 形式の `tag.Emoji` 込み連合)
- Reply / Move / Actor Update

実行前提:

- ``compose/docker-compose.federation-nekonoverse.yml`` の ``pytest`` profile
  が起動済み。``sakurasato_local_api`` (sks UDS + token) と
  ``nekonoverse_tokens`` (bob 用 OAuth Bearer, PR2b で導入) が pytest コンテナに
  見える状態。
- TUI バイナリは Dockerfile.tmux で ``/usr/local/bin/sakurasato-tui`` に
  焼かれている。
- ``SAKURASATO_TOKEN_FILE`` は ``sakurasato-token-issuer`` 1-shot コンテナが、
  ``NEKONOVERSE_TOKEN_FILE`` は ``nekonoverse-bob-issuer`` 1-shot コンテナが
  書いた状態。

flakiness 対策:

- 連合配送は worker 経由なので時間揺れがある (Nekonoverse / sakurasato 双方の
  キュー)。``poll_until`` で長め (90s) に待つ。
- tmux capture は polling ベース。`wait_until_text` は ``lib.sh`` の
  ``TMUX_E2E_POLL_INTERVAL`` (default 0.2s) で更新する。
"""
from __future__ import annotations

import uuid

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


# ── 共有 setup helpers (PR2b で追加) ──────────────────────────


@pytest.fixture(scope="session")
def bob_followed_by_sks(sakurasato: SakurasatoClient):
    """sks が bob を follow した accepted 状態を保証する session-scoped fixture。

    PR2b の `test_bob_note_appears_in_sks_timeline` は前提として「sks が bob を
    follow 済」が必要 (= bob の public note が sks の inbox に届くため)。
    `POST /api/v1/follow` は冪等 (PR #114) なので、PR2a の TUI 経由 follow
    が既に成立していても安全に再叩きできる ── session scope に倒して 1 回
    だけ叩くことで test 間の連合遅延蓄積を抑える。

    TUI 経路 (= PR2a の `:follow` コマンド) ではなく local API 直叩き経路を
    使う ── (a) TUI 起動を伴わないので fast、(b) PR2a test と test 順序が
    入れ替わっても挙動が変わらない、(c) PR2a が `:follow` 経路の責任を持つ。
    """
    # 既に follow 済なら no-op (`already_accepted=True` で返ってくる)。
    resp = sakurasato.follow(BOB_ACCT)
    follow_id = resp["follow_id"]

    # `already_accepted` が真なら poll 不要、即返す。新規 enqueue の場合は
    # state が `pending` から `accepted` に遷移するのを待つ。
    if resp.get("already_accepted"):
        yield {"follow_id": follow_id, "fresh": False}
        return

    def follow_accepted() -> bool:
        try:
            for f in sakurasato.following(limit=80):
                actor = f.get("actor") or {}
                host = (actor.get("host") or "").lower()
                name = (actor.get("preferred_username") or "").lower()
                if name == BOB_LOCAL and host == NEKONOVERSE_DOMAIN.lower():
                    return True
        except Exception:  # noqa: BLE001
            return False
        return False

    poll_until(follow_accepted, timeout=120, interval=2, desc="sks following bob (fixture setup)")
    yield {"follow_id": follow_id, "fresh": True}


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


# ── PR2b: bob 投稿 → sks home_timeline 受領 ───────────────────


def _post_marker() -> str:
    """テスト毎にユニークな本文 marker。

    短い hex (= UUID first 8) を末尾に付けて、過去テスト残骸 / 他テストとの
    取り違えを防ぐ。`hello PR2b post 1a2b3c4d` のような形。
    """
    return f"hello PR2b post {uuid.uuid4().hex[:8]}"


@pytest.mark.timeout(300)
def test_bob_note_appears_in_sks_timeline(
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """bob が nkv で投稿 → sks home timeline に federate されることを確認 (PR2b)。

    fixture `bob_followed_by_sks` で「sks → bob follow accepted」が成立済の状態。
    bob は public note を 1 本投稿し、sks の `/api/v1/timeline/home` を polling
    して同 note (URI 一致) が出現するまで待つ。

    TUI 経路は本シナリオでは使わない ── 受領 (inbox dispatch + DB 反映) の
    end-to-end を見たいだけなので、TUI 表示 layer を挟まずに最短経路で読む。
    TUI の note 描画自体は PR2a の `test_follow_bob_round_trips_to_accept` で
    `@me` ステータスバーや command prompt 経路で部分的にカバーされる。
    """
    _ = bob_followed_by_sks  # fixture 使用が分かるよう明示参照
    marker = _post_marker()

    # 1. bob 側で投稿。`uri` (AP id) は sks 側で `ap_id` 列に保存されるので
    #    一致 key にできる。
    posted = nekonoverse.create_status(marker, visibility="public")
    note_uri = posted["uri"]
    assert note_uri, f"create_status did not return uri: {posted}"

    # 2. sks home_timeline が note を取り込むまで polling。連合配送
    #    (nkv POST /inbox の Create + sks 側 inbox handler) + sks home TL
    #    取り込みの全フェーズで 60-90s 程度見ておく。
    def note_in_home() -> bool:
        try:
            timeline = sakurasato.home_timeline(limit=80)
        except Exception:  # noqa: BLE001
            return False
        for note in timeline:
            if note.get("ap_id") == note_uri:
                return True
            # `ap_id` が field 名違いで来た時のフォールバック
            if note.get("uri") == note_uri:
                return True
        return False

    poll_until(
        note_in_home,
        timeout=120,
        interval=3,
        desc=f"bob note {note_uri} in sks home timeline",
    )


# NOTE (PR2b 設計時): sks TUI から bob (= remote actor) の note に対する
# リアクション送信は `POST /api/v1/reactions` が
# `404 "reactions to remote notes are not supported yet"`
# (`crates/server/src/local_api/reactions.rs:77`) で弾かれる。TUI 側の
# `e` → `Enter` 経路 (= #118 で塞いだ binding) は本 PR の手動 tmux 駆動で
# fire することを確認済だが、server 側ギャップを別 PR で先に閉じる方が
# きれいなので、本 PR ではリアクション連合シナリオを実装しない。サーバ側が
# remote note への reaction を出せるようになった時点で、テストを生やす。
