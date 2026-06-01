"""Sakurasato TUI × Nekonoverse 連合シナリオ (#58 / #120 PR2a + PR2b + PR2c)。

PR2a (smoke 1 本):

  1. ``test_follow_bob_round_trips_to_accept``
     ── ``:`` で command prompt を開き ``:follow bob@nekonoverse`` → Accept
     round-trip までを確認する (= TUI 経路で follow が確立する)。

PR2b:

  2. ``test_bob_note_appears_in_sks_timeline``
     ── bob が nkv 側で note 投稿 → 連合配送で sks home timeline に出現する
     ことを確認 (= 取得側 = sks の inbox / TL の受領パス)。

PR2c (本 PR で追加):

  3. ``test_unicode_reaction_propagates_to_bob_status``
     ── bob が note 投稿 → sks TUI で ``e`` → ``+1`` → Enter → ``Like``
     activity を nkv inbox に配送し、bob 側 status で reaction (👍) として
     observable になることを確認する (= #118 Enter 回帰テスト + 配送経路)。
  4. ``test_custom_emoji_reaction_propagates_to_bob_status``
     ── 事前に sks に import 済みのテスト用 custom emoji を `e:sakurasato:Enter`
     で送り、``EmojiReact`` + ``tag.Emoji`` を nkv に届けて bob 側 status の
     reactions に並ぶことを確認する。

シナリオ外 (= 別 PR で追加 OK):

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

import os
import re
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


# ── PR2c: sks TUI → bob's remote note にリアクション → nkv 側 status 反映 ──
#
# PR2b 時点で塞がっていた server 側 `is_local` ガード (= "reactions to remote
# notes are not supported yet" 404) は PR #127 (`1bd72b6`) で撤去済 ──
# `crates/server/src/local_api/reactions.rs:73-80` の入口対称化により remote
# Note への `POST /api/v1/reactions` が 201 を返し、note 作者 inbox を含めて
# `delivery_queue` に積む経路が完成した。本 PR では TUI `e` 経路 (= #118 で
# `OpenEmojiSearch` に統合した binding) で実際に連合到達することを確認する。
#
# 観測モデル:
#
# Unicode `+1` (= 👍) の場合、sks は **`Like` activity** に `content: "👍"`
# を載せて配送する (`reactions.rs::build_reaction_activity` の Unicode 分岐)。
# Nekonoverse の `handlers/like.py::handle_like` は `content` が単一絵文字
# なら絵文字として保存するので、bob 側 `get_status` の `reactions` /
# `emoji_reactions` に 👍 1 件として並ぶ。`favourites_count` (= ⭐の数だけ
# 集計) ではなく `reactions` で観測する。
#
# Custom emoji `:sakurasato_test:` の場合、sks は **`EmojiReact`** + `tag:
# [Emoji]` + `_misskey_reaction` を載せる (`reactions.rs::build_reaction_activity`
# の Custom 分岐)。nkv の `handle_like` / `handle_emoji_react` は `content`
# / `_misskey_reaction` のどちらでも reaction に格納する経路があるので、
# bob 側 status の `reactions` に `:sakurasato_test:` 由来の 1 件が並ぶ。


# 検索クエリ → modal 内で先頭 (= prefix 一致トップ) に来る項目に絞れる
# 文字列を選ぶ。`+1` は `+1` (= 👍) 1 件で確定する。custom は env で渡される
# shortcode prefix (= `sakurasato_test` の `sakurasato` 部分) を打って
# `sakurasato_test` を先頭に呼ぶ。
UNICODE_QUERY = "+1"
UNICODE_CODEPOINT = "👍"
CUSTOM_SHORTCODE = os.environ.get("SAKURASATO_SEED_EMOJI_SHORTCODE", "sakurasato_test")
# `sakurasato_test` の前方一致が一意になる入力 (= `sakurasato` まで打てば
# 他の Unicode emoji や custom と衝突しない)。
CUSTOM_QUERY = CUSTOM_SHORTCODE.split("_", 1)[0]


def _open_tui_and_wait_for_note(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    marker: str,
    label: str,
):
    """TUI を起動し、bob 由来 note の `marker` が timeline に見えるまで待つ。

    `r` (refresh) を 1 度だけ叩くのは、TUI 起動直後の初回 fetch と federation
    着信のタイミングが揃わない場合の保険。bob note は `published_at` 最新で
    `ORDER BY n.id DESC` の先頭 (= `selected = 0`) に来るので、選択操作は不要。
    """
    tui = tmux_tui(
        sakurasato_socket_path,
        sakurasato_token_file,
        "--no-images",
        label=label,
    )
    tui.wait_until_text(r"@me", 30)
    tui.send_keys("r")
    # marker は `hello PR2c react-uni-xxxx` のような形なので、後半 hex を
    # 抜き出して厳格に当てる ── 同 stack 内の他テストが残した marker と
    # 取り違えないため。
    tui.wait_until_text(re.escape(marker), 60)
    return tui


def _bob_posts_and_waits_for_sks(
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
    marker: str,
) -> dict:
    """bob が note 投稿 → sks home_timeline に federate されるまで待つ。

    返り値: ``{"sks_note_id": int, "nkv_status_id": str, "ap_id": str,
    "marker": str}`` ── sks 側の local note id (= `note.id`) は TUI 経由の
    reaction 送出側からは不要だが、デバッグ時に grep しやすいので一緒に拾う。
    """
    posted = nekonoverse.create_status(marker, visibility="public")
    note_uri = posted["uri"]
    nkv_status_id = posted["id"]
    assert note_uri, f"create_status did not return uri: {posted}"

    def find_note():
        try:
            timeline = sakurasato.home_timeline(limit=80)
        except Exception:  # noqa: BLE001
            return None
        for note in timeline:
            if note.get("ap_id") == note_uri or note.get("uri") == note_uri:
                return note
        return None

    sks_note = poll_until(
        find_note,
        timeout=120,
        interval=3,
        desc=f"bob note {note_uri} federated into sks home timeline",
    )
    return {
        "sks_note_id": sks_note.get("id"),
        "nkv_status_id": nkv_status_id,
        "ap_id": note_uri,
        "marker": marker,
    }


def _nkv_status_has_reaction(
    nekonoverse: NekonoverseClient,
    status_id: str,
    *,
    needle: str,
) -> bool:
    """nkv `get_status` の reactions / emoji_reactions に `needle` 由来の
    1 件以上が見えるかを判定する。

    Mastodon 互換層は ``reactions`` (ReactionSummary[]) / ``emoji_reactions``
    (EmojiReaction[]) の 2 つで集計を返す。Unicode の場合 ``name`` が
    そのまま codepoint、custom の場合 ``:shortcode:`` 形式 (host suffix 込みの
    こともある)。どちらでも 1 件以上あれば成功にする。
    """
    try:
        status = nekonoverse.get_status(status_id)
    except Exception:  # noqa: BLE001
        return False
    for r in status.get("reactions") or []:
        name = r.get("name") or r.get("content") or ""
        if needle in name and r.get("count", 0) >= 1:
            return True
    for r in status.get("emoji_reactions") or []:
        name = r.get("name") or r.get("content") or ""
        if needle in name and r.get("count", 0) >= 1:
            return True
    return False


def _send_reaction_via_emoji_modal(tui, query: str) -> None:
    """TUI `e` で emoji 検索モーダルを開き、`query` を打って Enter で送出する。

    モーダルタイトルは ``  emoji search · react (<N>)  `` 形式なので、
    `emoji search` で待つ。candidate list は ``▶ <codepoint> :<shortcode>:``
    形式なので、Enter 後の status バー (= ``reacted with ...``) を確認する。
    """
    # Open the emoji search modal (= Issue #118 が Timeline `e` を
    # `OpenEmojiSearch` に統合した経路)。
    tui.send_keys("e")
    tui.wait_until_text(r"emoji search", 10)
    # query を 1 文字ずつ送る ── tmux send-keys は引数を逐次 literal で
    # 送ってくれるので、まとめて 1 引数で渡しても可。`+` を含むので shell
    # interpretation の罠を避けるため 1 文字 1 引数で送る。
    for ch in query:
        tui.send_keys(ch)
    tui.send_keys("Enter")
    # `send_reaction` の成功時 status: `reacted with <token> (<N> queued)`。
    # 失敗時 status: `reaction failed: ...`。前者で固定。
    tui.wait_until_text(r"reacted with", 15)


def _quit_tui(tui) -> None:
    """TUI を `:quit` で綺麗に閉じる。teardown でも kill されるので best-effort。"""
    try:
        tui.send_keys(":", "quit", "Enter")
        tui.wait_until_text(r"\$", 5)
    except Exception:  # noqa: BLE001
        pass


@pytest.mark.timeout(300)
def test_unicode_reaction_propagates_to_bob_status(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """sks TUI で bob の remote note に Unicode `+1` → nkv 側 reaction 反映。

    server 側の `is_local` ガード撤去 (PR #127) を、TUI `e` 経路 + 実 federation
    込みで end-to-end に検証する。
    """
    _ = bob_followed_by_sks
    marker = f"react-uni-{uuid.uuid4().hex[:8]}"
    fed = _bob_posts_and_waits_for_sks(sakurasato, nekonoverse, marker)

    tui = _open_tui_and_wait_for_note(
        tmux_tui,
        sakurasato_socket_path,
        sakurasato_token_file,
        marker,
        label="react_unicode",
    )
    try:
        _send_reaction_via_emoji_modal(tui, UNICODE_QUERY)

        poll_until(
            lambda: _nkv_status_has_reaction(
                nekonoverse, fed["nkv_status_id"], needle=UNICODE_CODEPOINT
            ),
            timeout=120,
            interval=3,
            desc=f"nkv status {fed['nkv_status_id']} got Unicode reaction {UNICODE_CODEPOINT}",
        )
    finally:
        _quit_tui(tui)


@pytest.mark.timeout(300)
def test_custom_emoji_reaction_propagates_to_bob_status(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """sks TUI で bob の remote note に custom emoji `:sakurasato_test:` → nkv 反映。

    `EmojiReact` + `tag.Emoji` + `_misskey_reaction` の 3 形式併載 (sks 側
    `build_reaction_activity` の custom 分岐) を Nekonoverse の `handle_like` /
    `handle_emoji_react` が拾えることを確認する。emoji は import 1-shot
    service (`sakurasato-emoji-import`) で事前に DB に乗っている前提。
    """
    _ = bob_followed_by_sks
    marker = f"react-custom-{uuid.uuid4().hex[:8]}"
    fed = _bob_posts_and_waits_for_sks(sakurasato, nekonoverse, marker)

    tui = _open_tui_and_wait_for_note(
        tmux_tui,
        sakurasato_socket_path,
        sakurasato_token_file,
        marker,
        label="react_custom",
    )
    try:
        _send_reaction_via_emoji_modal(tui, CUSTOM_QUERY)

        poll_until(
            lambda: _nkv_status_has_reaction(
                nekonoverse, fed["nkv_status_id"], needle=CUSTOM_SHORTCODE
            ),
            timeout=120,
            interval=3,
            desc=(
                f"nkv status {fed['nkv_status_id']} got custom reaction "
                f":{CUSTOM_SHORTCODE}:"
            ),
        )
    finally:
        _quit_tui(tui)
