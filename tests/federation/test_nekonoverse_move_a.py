"""Sakurasato ↔ Nekonoverse / Move Scenario A (#140 PR2)。

このテストファイルは **2-sks 連合 stack** 専用 ──
``compose/docker-compose.federation-nekonoverse-2sks.yml`` の ``pytest``
profile から発火する。1-sks 系の ``test_nekonoverse_tui.py`` とは compose
が異なるため、同じ container 内でも実行は完全に分離される。

## Scenario A の流れ

compose chain (= pytest 起動より前にすべて完了している前提):

1. ``alias-add-sks-old`` ── ``sakurasato-server alias add https://sakurasato-new/users/me``
2. ``alias-add-sks-new`` ── ``sakurasato-server alias add https://sakurasato/users/me``
   (= 双方向 ``alsoKnownAs`` 確立、``move-out`` の双方向検査を pass させる)
3. ``bob-follow-alice-trigger`` ── bob が alice@sakurasato に Follow を投入し
   accepted まで polling (= 移動元 alice の followers に bob が居る状態を
   担保。Move 配送先を確保する)
4. ``move-out-trigger`` ── ``sakurasato-server move-out https://sakurasato-new/users/me``
   が ``fetch_and_upsert`` + 双方向検査 + ``set_moved_to`` + ``delivery_queue``
   への Move activity enqueue を行う。配送自体は sakurasato-server daemon の
   delivery worker が非同期で実行する。

## pytest の責務

設置済みの compose chain が完了した時点 (= ``move-out-trigger`` の
``service_completed_successfully``) から、Move activity の配送と nkv 側
``handle_incoming_move`` の chain が完了するのを polling で観測する:

- nkv ``handle_incoming_move`` が bob の Follow を新規作成 (= alice@sks-new
  への Follow Activity を sks-new に配送)
- sks-new daemon が auto-Accept で応答
- nkv が Accept を受領して bob の follow を accepted に倒す
- bob の ``/api/v1/accounts/{bob_id}/following`` に alice@sakurasato-new が
  並ぶ

これ 1 観測点で Move 配送 + nkv 受領 + auto re-follow + sks-new Accept の
end-to-end chain を網羅する設計。

## 1-sks 系シナリオとの違い

- ``test_nekonoverse_tui.py`` は 1-sks + 1-nkv の compose (PR1 を含む)
- 本 file は 2-sks + 1-nkv の compose
- bob の token / nkv の host / certs は共通だが、sks-new 系 env (= ``SAKURASATO_NEW_*``)
  は本 stack のみで定義される

flake 対策:

- 連合配送 + handle_incoming_move + Follow Activity + Accept の往復で
  240s polling (= 1-sks 系 follow シナリオの 90s より長め)
- bob token は ``NEKONOVERSE_TOKEN_FILE`` で session-scope に load (既存
  ``nekonoverse`` fixture 流用)
"""
from __future__ import annotations

import os

import pytest

from conftest import (
    NEKONOVERSE_DOMAIN,
    NekonoverseClient,
    poll_until,
)


# Scenario A 固有の env (= 2sks compose の pytest service だけが定義する)。
SAKURASATO_NEW_DOMAIN = os.environ.get("SAKURASATO_NEW_DOMAIN", "sakurasato-new")
ALICE_NEW_ACCT = f"me@{SAKURASATO_NEW_DOMAIN}"


@pytest.mark.timeout(360)
def test_alice_move_a_propagates_bob_to_sks_new(
    nekonoverse: NekonoverseClient,
) -> None:
    """sks-old → sks-new Move が bob の following に sks-new alice を立てる
    ところまで到達することを確認する (#140 Scenario A の end-to-end)。

    pytest 起動時点で compose chain は以下を済ませている:
      - alias-add x 2 で双方向 alsoKnownAs 確立済
      - bob が alice@sakurasato を follow 済 (accepted)
      - sks-old で `sakurasato-server move-out` が完了 → Move activity が
        delivery_queue に積まれ、daemon が bob inbox へ配送中

    観測:
      - bob の `/api/v1/accounts/{bob_id}/following` に alice@sakurasato-new
        が並ぶまで 240s polling
      - `acct == "me@sakurasato-new"` で照合する (Mastodon 仕様)

    ## なぜ 240s か

    nkv ``handle_incoming_move`` 自体は同期処理だが、後段で立てる Follow を
    sks-new の inbox に配送する経路で再度 HTTP 署名 + 連合往復が走るため、
    end-to-end のレイテンシは「Mastodon-compat な Follow 往復」より長い。
    既存 ``bob_followed_by_sks`` fixture も 90-120s 程度待つので、その 2-3
    倍を見て 240s に倒す。
    """
    # 1. bob 自身の nkv-local id を取り出す (`verify_credentials` 経由)。
    #    Follow / Following の API は本人 token で叩く形でも内部 id は同じ。
    me = nekonoverse.verify_credentials()
    bob_id = me.get("id")
    assert bob_id, f"verify_credentials did not return account id: {me}"

    # 2. bob の following を polling して alice@sakurasato-new を待つ。
    def alice_new_in_bob_following() -> bool:
        try:
            following = nekonoverse.following(bob_id)
        except Exception:  # noqa: BLE001
            return False
        target = ALICE_NEW_ACCT.lower()
        for entry in following:
            # `acct` は Mastodon spec で `username@domain` を返す (remote actor)。
            # `url` には actor の AP id (例: `https://sakurasato-new/users/me`)。
            acct = (entry.get("acct") or "").lower()
            url = (entry.get("url") or "").lower()
            if acct == target:
                return True
            # fallback: acct が `me@<resolved-host>` 表記で揺れる場合のため、
            # url の host 部一致で救済する。
            if SAKURASATO_NEW_DOMAIN.lower() in url and "/users/me" in url:
                return True
        return False

    poll_until(
        alice_new_in_bob_following,
        timeout=240,
        interval=3,
        desc=(
            f"bob following includes alice ({ALICE_NEW_ACCT}) "
            "after sks-old → sks-new Move propagation"
        ),
    )
