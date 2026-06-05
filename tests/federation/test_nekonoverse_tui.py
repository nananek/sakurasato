"""Sakurasato TUI × Nekonoverse 連合シナリオ (#58 / #120 PR2a + PR2b + PR2c + #138)。

PR2a (smoke 1 本):

  1. ``test_follow_bob_round_trips_to_accept``
     ── ``:`` で command prompt を開き ``:follow bob@nekonoverse`` → Accept
     round-trip までを確認する (= TUI 経路で follow が確立する)。

PR2b:

  2. ``test_bob_note_appears_in_sks_timeline``
     ── bob が nkv 側で note 投稿 → 連合配送で sks home timeline に出現する
     ことを確認 (= 取得側 = sks の inbox / TL の受領パス)。

PR2c:

  3. ``test_unicode_reaction_propagates_to_bob_status``
     ── bob が note 投稿 → sks TUI で ``e`` → ``+1`` → Enter → ``Like``
     activity を nkv inbox に配送し、bob 側 status で reaction (👍) として
     observable になることを確認する (= #118 Enter 回帰テスト + 配送経路)。
  4. ``test_custom_emoji_reaction_propagates_to_bob_status``
     ── 事前に sks に import 済みのテスト用 custom emoji を `e:sakurasato:Enter`
     で送り、``EmojiReact`` + ``tag.Emoji`` を nkv に届けて bob 側 status の
     reactions に並ぶことを確認する。

#138:

  5. ``test_sks_tui_reply_propagates_to_nkv_descendants``
     ── bob が公開 note → sks TUI で ``R`` で reply prompt → 本文入力 → F2 で
     送出 → bob 側 ``status_context`` の ``descendants`` に sks 由来 reply が
     出現することを確認 (= ``in_reply_to_ap_id`` 永続化 + mention 自動付与 +
     ``Compose`` の reply target セット経路の end-to-end)。
  6. ``test_nkv_reply_appears_in_sks_timeline_with_in_reply_to``
     ── sks alice が公開 note → bob (= nkv) が ``lookup_status`` で local
     status id を引き、``in_reply_to_id`` 付きで reply 投稿 → sks home
     timeline で同 reply が ``in_reply_to_ap_id`` 一致して出現することを
     確認 (= 受信側 inbox handler が reply の親紐付けを保持する経路)。

#139 (本 PR で追加):

  7. ``test_sks_tui_avatar_upload_updates_actor_icon``
     ── sks TUI で ``A`` (アバターアップロード) → 一時 fixture dir 内の PNG を
     picker 経由で選択 → アップロード完了 → SKS 側 ``whoami.icon_url`` が
     新 URL に更新され、同 URL が AP ``/users/<name>`` の ``icon.url`` にも
     反映されていることを確認 (= TUI key → media-proxy sanitize → versitygw
     格納 → ``profile.rs::patch`` → AP serving の end-to-end chain)。
     federation push (= alice 自身の Update activity を bob inbox に届け、
     nkv 側 cache を更新するパス) の検証は別 PR で扱う ── bob → alice
     follow fixture が必要だが、現状 ``bob_followed_by_sks`` の逆方向 helper
     が conftest に居ないため、まず最重要の SKS 側 chain を切り出して
     検証する。

#140 PR1 (本 PR で追加):

  8. ``test_bob_move_to_bob_new_propagates_to_sks_following``
     ── Scenario B (nkv → sks Move): bob_new (= 2nd nkv account) をテスト内で
     登録 → bob_new に ``also_known_as=[bob_ap_id]`` を立てる → bob として
     ``POST /api/v1/accounts/move`` で bob_new に引っ越し → sks 側
     ``/api/v1/following`` に bob_new が並ぶまで待つ (= sks ``handle_move`` +
     auto re-follow + nkv Accept の end-to-end)。compose を 2-sks 拡張せず
     既存 1-sks + 1-nkv のまま実行できる。

シナリオ外 (= 別 PR で追加 OK):

- Move Scenario A (#140 PR2): sks-old → sks-new。compose に 2nd sks を立てる
  必要がありインフラ拡張あり。
- Avatar Update の nkv side 検証 (= follower push の確認)

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
import secrets
import shutil
import struct
import uuid
import zlib
from pathlib import Path

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
# #140 Scenario B の Move target。`nekonoverse-bob-new-issuer` (compose) が
# 固定 username `bobnew` で登録 + oauth_tokens 直 seed しておく ── ここの値は
# compose の `BOB_USERNAME: bobnew` と一致させること。
BOB_NEW_LOCAL = "bobnew"
BOB_NEW_ACCT = f"{BOB_NEW_LOCAL}@{NEKONOVERSE_DOMAIN}"


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


# ── #138: Reply 双方向 ────────────────────────────────────────
#
# シナリオ A (sks → nkv) と B (nkv → sks) の 2 方向で reply の連合パスを
# 検証する。Reaction PR2c とは独立に、`in_reply_to_ap_id` の永続化と
# mention 自動付与 (= [`crates/server/src/local_api/notes.rs::recipients_for`])
# が「親 note 著者 inbox を to に含めて配送」する経路を踏む。
#
# 観測モデル:
#
# A. sks TUI で bob remote note に reply ── Compose の reply target が
#    `Note.inReplyTo = <bob note ap_id>` として AP `Create.object` に乗り、
#    Nekonoverse 側で descendants として bob のスレッドに刺さる。
#    Mastodon `GET /api/v1/statuses/{id}/context` で descendants を取れる。
#
# B. bob (nkv) が `in_reply_to_id` 付きで alice (sks) note に reply ── sks
#    inbox handler が `Create/Note` を取り込み、`note.in_reply_to_ap_id` に
#    alice の note ap_id を保存する (M11 で完成済の経路)。home_timeline 経由
#    で観察、`in_reply_to_ap_id` 一致を assert する。


def _open_reply_prompt_from_top(tui, parent_marker: str) -> None:
    """Timeline focus で `R` を打って **選択中** (= 先頭) note の reply を開く。

    bob 由来 note は `_open_tui_and_wait_for_note` が refresh 後に先頭 (= id
    最大 = published 最新) で待っているので、追加の選択操作なしで `R` だけで
    その note への reply prompt が開く。

    ただし「先頭 = bob の note」前提を rely 一本足にすると、将来テストの
    並列化や別 stack 内のタイミング揺れで誤った note に reply する事故が
    起き得る (= PR #184 review concern 1)。そのため `R` を打つ直前に
    `parent_marker` が画面に**まだ**居ることを `wait_until_text` で再確認
    する ── 選択カーソル位置までは観察できないが、bob の note が timeline
    から消えていないことは担保できる。

    Compose 上部の「↳ @<author>: <抜粋>」ラベル (= `Compose::set_reply_target`
    後のレンダリング) を ``↳ @bob`` で待ち、reply 開始が成立したことを確認する。
    """
    # parent_marker がまだ可視であることを確認 (= 先頭 selection の暗黙
    # 前提への safety net、PR #184 review concern 1)。
    tui.wait_until_text(re.escape(parent_marker), 5)
    tui.send_keys("R")
    # `↳ @<bob_local>` を本文末尾抜粋と一緒に当てる ── `↳` は ratatui の
    # `set_reply_target` 後のヘッダで `compose.rs` の reply_parent_label を
    # 表示するときに付ける prefix (= `format!("↳ {label}")`, `ui/mod.rs` の
    # render_compose 経路)。
    #
    # timeout は他 polling と整合させて 30s。tmux pty + ratatui 描画は
    # まれにフレームが遅れることがある (= PR #184 review concern 2、旧
    # 10s だと CI 負荷時に false-fail のリスク)。
    tui.wait_until_text(rf"↳ @{BOB_LOCAL}", 30)


def _type_compose_body_and_submit(tui, body: str) -> None:
    """Compose 本文に `body` を入力して F2 で送出する。

    F2 を使うのは tmux 上の VT 端末で Ctrl+Enter が CSI u (Kitty keyboard
    protocol) でしか届かないため (= [`crates/tui/src/event.rs`] の
    `KeyCode::F(2) => Action::SubmitNote` 代替経路。CLAUDE.md の M3b 経緯と
    揃え)。
    """
    for ch in body:
        tui.send_keys(ch)
    tui.send_keys("F2")
    # 送出成功時 status: `posted #<id> (<N> delivered)`。失敗時は
    # `post failed: ...`。前者を厳密に待つ。
    #
    # 正規表現は **POSIX ERE** で書く ── `wait_until_text` は lib.sh 側で
    # `grep -Eq` に渡す。GNU grep の ERE は `\d` を数字クラスとして解釈せず
    # 「stray \ before d」警告付きでリテラル `d` に倒すため、`posted #\d+` は
    # 実際の `posted #5 ...` に**一致しない** (= ubuntu runner で 20s timeout)。
    # ローカルの ugrep / busybox grep は `\d` を数字に解釈するので開発機では
    # 通り、CI だけ落ちる罠だった。`[0-9]+` なら 3 実装すべてで一致する。
    tui.wait_until_text(r"posted #[0-9]+", 20)


def _descendants_contain_marker(
    nekonoverse: NekonoverseClient,
    parent_status_id: str,
    *,
    marker: str,
) -> bool:
    """bob 側 `status_context.descendants` に marker 本文を持つ status が居るか。

    nkv は Mastodon 仕様で `content` に HTML を入れる (= 本文をラップした
    `<p>...</p>`)。marker 文字列 (= `reply-A-<hex>` 等) は ASCII のみで
    HTML エスケープを踏まないので、`in` 部分一致で十分。1 件以上見つかれば
    成功にする ── 連合経路で重複配送がもし発生しても (idempotent insert で
    重複行は出ない想定だが念のため) 通る。
    """
    try:
        ctx = nekonoverse.status_context(parent_status_id)
    except Exception:  # noqa: BLE001
        return False
    for d in ctx.get("descendants") or []:
        content = d.get("content") or ""
        if marker in content:
            return True
    return False


@pytest.mark.timeout(360)
def test_sks_tui_reply_propagates_to_nkv_descendants(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """#138 シナリオ A: sks TUI から bob remote note への reply を nkv 側で確認。

    Compose の reply target セット経路 + `in_reply_to_ap_id` の wire + AP
    `Create.object.inReplyTo` の解釈を end-to-end で踏む。
    """
    _ = bob_followed_by_sks
    parent_marker = f"reply-A-parent-{uuid.uuid4().hex[:8]}"
    fed = _bob_posts_and_waits_for_sks(sakurasato, nekonoverse, parent_marker)

    reply_marker = f"reply-A-{uuid.uuid4().hex[:8]}"
    tui = _open_tui_and_wait_for_note(
        tmux_tui,
        sakurasato_socket_path,
        sakurasato_token_file,
        parent_marker,
        label="reply_sks_to_nkv",
    )
    try:
        _open_reply_prompt_from_top(tui, parent_marker)
        _type_compose_body_and_submit(tui, reply_marker)

        poll_until(
            lambda: _descendants_contain_marker(
                nekonoverse, fed["nkv_status_id"], marker=reply_marker
            ),
            timeout=180,
            interval=3,
            desc=(
                f"nkv status {fed['nkv_status_id']} got reply {reply_marker!r} "
                "in descendants"
            ),
        )
    finally:
        _quit_tui(tui)


def _sks_note_with_in_reply_to(
    sakurasato: SakurasatoClient,
    *,
    parent_ap_id: str,
    marker: str,
) -> dict | None:
    """sks home_timeline で `marker` 本文 + `in_reply_to_ap_id == parent_ap_id`
    を満たす note を返す。条件を満たさなければ ``None``。

    home_timeline は新しい note を先頭に積むので、limit 80 で初期 horizon を
    覆える前提。reply 1 件 + 親 1 件 + 自分の post 程度しかない pytest 環境では
    十分 ── 万一を考えて marker を本文に持つ条件で絞り、他テストの post と
    取り違えないようにする。
    """
    try:
        timeline = sakurasato.home_timeline(limit=80)
    except Exception:  # noqa: BLE001
        return None
    for note in timeline:
        if note.get("in_reply_to_ap_id") != parent_ap_id:
            continue
        content = note.get("content") or ""
        if marker in content:
            return note
    return None


@pytest.mark.timeout(360)
def test_nkv_reply_appears_in_sks_timeline_with_in_reply_to(
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
) -> None:
    """#138 シナリオ B: bob → alice reply が sks home timeline に `in_reply_to_ap_id`
    付きで届くことを確認する。

    sks は local API 直叩きで親 note を post し、bob は ``lookup_status`` で
    nkv 側の local status id を引いてから ``in_reply_to_id`` 付きで reply を
    投稿する。TUI 経路は使わない (= 受信側 inbox handler の責務だけを見たい)。
    """
    _ = bob_followed_by_sks
    parent_marker = f"reply-B-parent-{uuid.uuid4().hex[:8]}"
    # 1. sks alice が公開 note を投稿。
    parent_note = sakurasato.create_note(parent_marker, visibility="public")
    parent_ap_id = parent_note.get("ap_id") or parent_note.get("uri")
    assert parent_ap_id, f"sks create_note did not return ap_id: {parent_note}"

    # 2. nkv 側で resolve させて、bob から見える local status id を引く。
    #    nkv が configure on-demand fetch なので resolve=true で確実に
    #    取り込む。配送経由でも来るが、双方競合しても idempotent insert で
    #    1 行に収束する (nkv 側挙動の前提)。
    def looked_up() -> dict | None:
        try:
            return nekonoverse.lookup_status(parent_ap_id)
        except Exception:  # noqa: BLE001
            return None

    nkv_parent = poll_until(
        looked_up,
        timeout=120,
        interval=3,
        desc=f"nkv resolves sks parent {parent_ap_id} into local status",
    )
    nkv_parent_id = nkv_parent["id"]

    # 3. bob (nkv) が reply を投稿。`in_reply_to_id` は nkv 側 local id。
    reply_marker = f"reply-B-{uuid.uuid4().hex[:8]}"
    nekonoverse.create_status(
        reply_marker, visibility="public", in_reply_to_id=nkv_parent_id
    )

    # 4. sks 側に reply が `in_reply_to_ap_id` 一致で federate される。
    #    親が sks 自身の note なので mention 自動付与 (recipients_for) で
    #    alice 宛 to が乗り、inbox handler が取り込み + home_timeline 経由で
    #    観察できる。
    poll_until(
        lambda: _sks_note_with_in_reply_to(
            sakurasato, parent_ap_id=parent_ap_id, marker=reply_marker
        ),
        timeout=180,
        interval=3,
        desc=(
            f"bob reply {reply_marker!r} appears in sks home timeline "
            f"with in_reply_to_ap_id={parent_ap_id}"
        ),
    )


# ── #139: Avatar Update ──────────────────────────────────────
#
# TUI `A` で avatar をアップロードし、SKS 側 chain
# (TUI → local API `/api/v1/media?kind=avatar` → media-proxy sanitize →
# versitygw 格納 → `profile.rs::patch` → `actor.icon_url` 更新 → AP serving)
# が end-to-end で動くことを確認する。
#
# 観測モデル:
#
# 1. 上げる前の `whoami.icon_url` を snapshot しておく (初期状態は通常 None)。
# 2. TUI で `A` → 一時 picker root dir を経由して PNG を選択 → `avatar updated`
#    status を待つ。
# 3. 上げた後 `whoami.icon_url` が **変化** していることを確認。
# 4. AP `/users/<name>` (= bob/nkv が `Update` 後に fetch する URL) を直接叩き、
#    `icon.url` が `whoami.icon_url` と一致することを確認 ── これで nkv 側
#    cache 反映の **前提** (= sks が正しい新 URL を AP で serve する) が成立。
#
# nkv 側 cache 更新 (= alice の `Update` を bob 経由で受領しているか) は別 PR。
# 現状 conftest に「bob が alice を follow」の helper が無いため (= 既存
# `bob_followed_by_sks` は **sks → bob** の片方向)、push 観測のための fixture
# は別途整備する。本 PR では SKS 側 chain の最重要部分を切り出して固める。

def _build_unique_1x1_png() -> bytes:
    """画素値ランダムな 1x1 RGB PNG を stdlib だけで合成する。

    なぜランダム化するか:

    media-proxy は受け取った画像を ``image`` クレートで decode → avatar
    variant (256x256) に resize → WebP 再エンコードする ([`crates/media-proxy/src/image_pipeline.rs`])。
    versitygw に格納される storage key は WebP の SHA-256 が入るので、
    **入力 PNG が同じなら毎回同じ URL** が返る。本テストが「`icon_url` が
    変化した」ことを観測する設計上、テストを 2 回以上同じ DB で走らせると
    upload 前後で URL が同一になって false negative する。色をランダム化
    することで WebP 出力も毎回ユニークになり、URL が衝突しない。

    PNG 仕様 (RFC 2083) に従い、IHDR / IDAT / IEND の 3 chunk を CRC 付きで
    手書きする。スコープを 1x1 に絞ることで IDAT は filter byte 1 + RGB 3
    バイトの計 4 バイトに収まる ── ``image`` クレートのデコーダも最小だが
    valid な PNG として受け入れる (= 過去 PR2c の sanitize 経路と同じ)。
    """
    width = height = 1
    color = (
        secrets.randbelow(256),
        secrets.randbelow(256),
        secrets.randbelow(256),
    )

    def _chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(
            ">I", zlib.crc32(tag + data)
        )

    sig = b"\x89PNG\r\n\x1a\n"
    # IHDR: width(4) height(4) bit_depth(1) color_type(1=RGB) compression(0)
    # filter(0) interlace(0). color_type=2 = RGB without alpha。
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    # IDAT: 各 scanline の頭に filter type byte (0=None)、続いて RGB バイト列。
    raw = b"\x00" + bytes(color)
    idat = zlib.compress(raw, level=9)
    return sig + _chunk(b"IHDR", ihdr) + _chunk(b"IDAT", idat) + _chunk(b"IEND", b"")


def _prepare_picker_fixture(stem: str = "avatar") -> tuple[Path, Path]:
    """`/tests/0_<stem>_picker_<uuid>/<stem>.png` を作って (dir, file) を返す。

    Picker は alphabetic sort で entries を並べ、dirs を先に出す
    ([`crates/tui/src/picker.rs::try_read_dir`])。dir 名を ``0_`` で始めることで:

    - ``__pycache__`` (`_` = 0x5F) や ``D``/``a``/`` から始まる既存 file/dir よりも
      先頭 (= 0x30 < 0x5F < 0x41 etc.) に位置決めできる。
    - 隠し dir (``.pytest_cache`` 等) は `show_hidden=false` で picker から見えない。

    結果として TUI 側で ``A`` → ``j`` (= 先頭 dir を選択) → ``Enter`` (= 降りる)
    → ``j`` (= 内側の唯一の file を選択) → ``Enter`` (= 確定) の 5 打鍵で
    fixture 画像を upload できる。
    """
    base_dir = Path("/tests")
    unique = uuid.uuid4().hex[:8]
    dir_path = base_dir / f"0_{stem}_picker_{unique}"
    dir_path.mkdir(exist_ok=True)
    file_path = dir_path / f"{stem}.png"
    file_path.write_bytes(_build_unique_1x1_png())
    return dir_path, file_path


def _select_fixture_in_picker(tui) -> None:
    """`A` → picker → 「先頭 dir に descend → 内側の唯一の file を select」。

    `_prepare_picker_fixture` で作った dir/file レイアウト前提。タイミング
    安全のため、各ステップで status / dir 表示の固定マーカーを待つ。

    - status「`file picker: avatar (Enter=select, Esc=cancel)`」(= `open_picker`
      の `set_status`) を `file picker: avatar` で待つ。
    - dir descend 後はカレントパス表示 (= `render_picker` でヘッダに `cwd` を出す
      想定) と、内側の単一 file 名を表示で待つ。
    """
    tui.send_keys("A")
    tui.wait_until_text(r"file picker: avatar", 10)
    # `j` → 先頭 dir 選択 → `Enter` で descend。
    tui.send_keys("j", "Enter")
    # 降りた dir には fixture file 1 個しか無いので `j` + `Enter` で confirm。
    tui.send_keys("j", "Enter")
    # 成功 status: `avatar updated (<N> delivered)` (= `handle_upload_outcome`
    # の Success ブランチ、N は accepted follower 数)。
    tui.wait_until_text(r"avatar updated", 60)


@pytest.mark.timeout(360)
def test_sks_tui_avatar_upload_updates_actor_icon(
    tmux_tui,
    sakurasato_socket_path: str,
    sakurasato_token_file: str,
    sakurasato: SakurasatoClient,
) -> None:
    """#139: TUI `A` → アバター更新 → SKS の local API + AP actor JSON が新 URL を返す。

    本 test は **`bob_followed_by_sks` fixture に依存しない** ── 既存
    fixture は ``sks → bob`` の片方向 follow で「alice の follower 集合」
    には bob が入らない (= [`crates/server/src/local_api/profile.rs::enqueue_to_followers`]
    は `list_accepted_inboxes(alice.id)` を引き、これは「alice を follow
    している人」を返す)。そのためこの fixture は本テストの federation push
    観測には貢献しない。

    push 観測 (= alice の Update を bob inbox に届け、nkv 側 cache が更新
    される) は別 PR で扱う ── 必要な「bob → alice follow」helper が conftest
    に居ないため、まず最重要の SKS 側 chain を切り出して検証する。本テスト
    自体は ``avatar updated (0 delivered)`` 表示 (= 0 follower) でも green
    する ── status 文字列の数値部は assert していない。
    """
    # 1. 上げる前の icon_url を snapshot。
    before = sakurasato.whoami()
    icon_before = before.get("icon_url")

    # 2. picker 用の fixture を `/tests/0_avatar_picker_<uuid>/avatar.png` に書く。
    fixture_dir, fixture_file = _prepare_picker_fixture(stem="avatar")

    tui = tmux_tui(
        sakurasato_socket_path,
        sakurasato_token_file,
        "--no-images",
        label="avatar_upload",
    )
    try:
        # TUI 起動して whoami が描画される (= status バー / Timeline) のを待つ。
        tui.wait_until_text(r"@me", 30)
        _select_fixture_in_picker(tui)

        # 3. whoami が new URL を返す (= local API 経由で server の actor.icon_url
        #    が更新済)。
        after = sakurasato.whoami()
        icon_after = after.get("icon_url")
        assert icon_after, f"whoami.icon_url should be non-empty after upload: {after}"
        assert icon_after != icon_before, (
            f"whoami.icon_url should change after avatar upload "
            f"(before={icon_before!r}, after={icon_after!r})"
        )

        # 4. AP `/users/<name>` (= bob/nkv が fetch する URL) も同じ URL を出す。
        whoami_username = before.get("preferred_username") or "me"
        actor = sakurasato.actor_json(whoami_username)
        icon_obj = actor.get("icon") or {}
        ap_icon_url = icon_obj.get("url")
        assert ap_icon_url == icon_after, (
            f"AP actor.icon.url should match whoami.icon_url "
            f"(ap={ap_icon_url!r}, whoami={icon_after!r})"
        )
    finally:
        _quit_tui(tui)
        # picker fixture を後始末 (= 後続テストの /tests/ list を汚さない)。
        shutil.rmtree(fixture_dir, ignore_errors=True)
        # fixture_file は dir 削除で連鎖的に消える ── ループ内で個別 unlink せず。
        _ = fixture_file


# ── #140 PR1 (Scenario B): nkv outbound Move → sks 側 alice 自動 re-follow ──
#
# Issue #140 の Scenario B = 「Nekonoverse 側 outbound Move」を CI で駆動する。
# 過去 (= 起票時点) は "Nekonoverse が Move outbound に対応していれば" として
# blocked の脇に並んでいたが、Nekonoverse develop (= 20260602-3 系列) には
# `app/services/move_service.py::initiate_move` + Mastodon-compat な
# `POST /api/v1/accounts/move` が揃っているため、sks 側 `handle_move` を実機
# 駆動できる。
#
# Scenario A (sks-old → sks-new) は compose を 2-sks に拡張する別 PR でやる。
# 本シナリオは現行 1-sks + 1-nkv compose のまま、bob_new (= 2nd nkv account)
# を test 内で実行時に生やす経路で完結する。
#
# 観測対象 (= sks `handle_move`):
# - bob (= signer) の actor_row に `moved_to_ap_id = bob_new ap_id` が立つ
# - bob を follow している local actor (= alice@sks) が bob_new に自動 Follow
#   を `delivery_queue` 経由で送出 → nkv が Accept → sks `/api/v1/following`
#   に bob_new が現れる
#
# 観測点としては「`/api/v1/following` に bob_new が並ぶ」だけで上記 2 件を間接的
# にカバーする ── bob_new ap_id を出すには (a) sks が bob_new を fetch + insert
# (b) auto re-follow が delivery + accept まで成立、の両方が必要。


@pytest.mark.timeout(360)
def test_bob_move_to_bob_new_propagates_to_sks_following(
    bob_followed_by_sks,
    sakurasato: SakurasatoClient,
    nekonoverse: NekonoverseClient,
    nekonoverse_bob_new_token: str,
) -> None:
    """nkv → sks Move (Scenario B): bob が bob_new に引っ越すと alice の
    follow が bob_new に追従する (= sks `handle_move` + auto re-follow)。

    フェーズ:
      1. `bob_followed_by_sks` fixture で alice@sks → bob@nkv accepted を確保。
      2. Move target `bobnew` (= 2nd nkv user) は `nekonoverse-bob-new-issuer`
         が起動時に登録 + oauth_tokens 直 seed 済み。Nekonoverse は
         `/api/v1/accounts` が token を返さない (seed-bob.sh 参照) ので、
         bob と同じく専用 issuer が DB-seed した Bearer を fixture
         (`nekonoverse_bob_new_token`) 経由でファイル受領する。
      3. bob_new として `PATCH /accounts/update_credentials` で
         `also_known_as=[bob_ap_id]` を立てる (= sks 側 `handle_move` の
         双方向同意チェックで必須)。
      4. bob として `POST /accounts/move` で target=bob_new ap_id を渡す。
      5. sks `/api/v1/following` を polling し、`bob_new@nekonoverse` が
         accepted 一覧に現れるまで待つ (= Move 受領 + auto re-follow + nkv
         側 Accept 配送 + sks 側 state 遷移の end-to-end)。
    """
    _ = bob_followed_by_sks  # fixture 使用が分かるよう明示参照

    # 1. bob の AP id (= Move の source) を `webfinger` から決定論的に引く。
    #    nkv の WebFinger は `aliases[]` に actor URI を載せる Mastodon 慣行。
    wf = nekonoverse.webfinger(BOB_ACCT)
    bob_ap_id = next(
        (
            link["href"]
            for link in wf.get("links", [])
            if link.get("rel") == "self" and link.get("type", "").startswith("application/")
        ),
        None,
    )
    assert bob_ap_id, f"WebFinger did not return self link with AP type: {wf}"

    # 2. Move target `bobnew` は issuer コンテナが登録 + token seed 済み。
    #    Bearer は fixture (= 共有 volume の `bob_new.token`) から受け取る。
    bob_new_token = nekonoverse_bob_new_token

    # bob_new の AP id を WebFinger 経由で引く (= 登録 → AP actor URI の確立を
    # 待つ意味も兼ねる)。Nekonoverse の AP URI 形は `https://nekonoverse/users/<id>`
    # 系なので、文字列構築より WebFinger に任せた方が安全。
    bob_new_acct = BOB_NEW_ACCT

    def bob_new_webfinger_resolves() -> str | None:
        try:
            wf_new = nekonoverse.webfinger(bob_new_acct)
        except Exception:  # noqa: BLE001
            return None
        return next(
            (
                link["href"]
                for link in wf_new.get("links", [])
                if link.get("rel") == "self" and link.get("type", "").startswith("application/")
            ),
            None,
        )

    bob_new_ap_id: str | None = None

    def _resolved() -> bool:
        nonlocal bob_new_ap_id
        bob_new_ap_id = bob_new_webfinger_resolves()
        return bob_new_ap_id is not None

    poll_until(
        _resolved,
        timeout=30,
        interval=1,
        desc=f"WebFinger for bob_new ({bob_new_acct}) resolves to AP URI",
    )
    assert bob_new_ap_id is not None

    # 3. bob_new 側で `also_known_as=[bob_ap_id]` を立てる。Nekonoverse は Form
    #    入力で受けるので `multipart/form-data` で JSON 配列文字列を送る。
    nekonoverse.update_credentials_also_known_as(
        token=bob_new_token,
        also_known_as=[bob_ap_id],
    )

    # 4. bob として Move を起動する。`POST /accounts/move` は nkv 側で
    #    target.alsoKnownAs を fresh fetch + 検証 → 自身 movedTo を立て →
    #    followers (alice@sks) 全 inbox に Move 配送を enqueue する。
    #
    #    成功判定は `initiate_move` 内の `raise_for_status` に任せる ──
    #    Nekonoverse develop は `{"ok": true}` を返すが、本テストはレスポンス
    #    body の中身に依存せず HTTP 200 だけを契機にする (PR #194 round-1
    #    🔴 対応: Mastodon 仕様の空オブジェクト返却に揺れても壊れない)。
    nekonoverse.initiate_move(
        token=nekonoverse.token,
        target_ap_id=bob_new_ap_id,
    )

    # 5. sks 側 `/api/v1/following` を polling して、bob_new が現れるのを待つ。
    #    観測経路:
    #      a) sks `inbox` で Move を受領 → `handle_move`
    #      b) `handle_move` が `ensure_target_actor` で bob_new を fresh fetch
    #         + alsoKnownAs 検証
    #      c) `set_moved_to` で bob の moved_to=bob_new_ap_id
    #      d) `enqueue_auto_refollow` で alice → bob_new の Follow を queue
    #      e) sks worker が Follow を nkv (bob_new inbox) に配送
    #      f) nkv が Accept を返送
    #      g) sks `accept` 受領で follow.state = accepted
    #      h) `/api/v1/following` に bob_new が並ぶ
    #
    #    nkv → sks Move 配送 (= b 以前のフェーズ) + 上記 e..g の往復で総時間
    #    が嵩むため、timeout は 180s に倒す。30s 程度の余白を見ておく。
    def bob_new_in_sks_following() -> bool:
        try:
            following = sakurasato.following(limit=120)
        except Exception:  # noqa: BLE001
            return False
        for entry in following:
            actor = entry.get("actor") or {}
            host = (actor.get("host") or "").lower()
            name = (actor.get("preferred_username") or "").lower()
            if name == creds["username"].lower() and host == NEKONOVERSE_DOMAIN.lower():
                return True
        return False

    poll_until(
        bob_new_in_sks_following,
        timeout=180,
        interval=3,
        desc=(
            f"sks following includes bob_new ({creds['username']}@{NEKONOVERSE_DOMAIN}) "
            "after nkv→sks Move propagation"
        ),
    )
