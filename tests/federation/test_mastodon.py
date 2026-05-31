"""Mastodon ↔ Sakurasato programmatic federation tests.

カバー範囲 (Issue #56 の指定):

- TestHealth           ── 両 instance の `/healthz` / NodeInfo
- TestWebFinger        ── 相互の `acct:` 解決
- TestActor            ── 相互の actor JSON 取得
- TestFollow           ── Sakurasato → Mastodon (`sakurasato-prefollow-bob` 経由) と
                          Mastodon → Sakurasato (`accounts/search?resolve=true` + follow)
- TestNoteFromSks      ── Sakurasato 投稿 → Mastodon Bob の home timeline に届く
- TestNoteFromMastodon ── Mastodon 投稿 → Sakurasato Me の home timeline に届く (= #55 で実装)
- TestReactionInbound  ── Mastodon Bob の Favourite (= Like) が Sakurasato 側 reactions に反映
- TestMoveSkip         ── alsoKnownAs + Move は 2nd Mastodon account が要るため将来 PR で

ポイント:

- Sakurasato 側操作は **すべて Bearer 認証付きの UDS local API** 経由。公開 TCP
  には載っていない `/api/v1/*` を本来の経路で触る。
- Mastodon 側操作は OAuth password grant でログインした `bob` で行う。
- federation は非同期 (delivery worker + Sidekiq) なので、`poll_until` で最大
  60 秒待つ。
- 連合経路は **side-effect で観測** する (= Bob が home_timeline に load し直して
  確認する等)。直接 DB を覗かない。

ordering 依存 (重要):

- `TestNoteFromSakurasato` / `TestNoteFromMastodon` / `TestReactionInbound` は
  **`TestFollow` が走ったあと** に動くことが期待される (Mastodon Bob と
  Sakurasato Me 双方向のフォローが accepted になっている前提で進む)。
- pytest のデフォルト collection 順 (= 定義順) で正しく並ぶように書いてあるが、
  `pytest-randomly` 等で順序がかき混ぜられると依存が崩れ、180s poll の
  保険でも吸収しきれない可能性がある。order を変える場合は各テストの
  冒頭で `mastodon.follow(...)` を再度叩いて、暗黙の前提を idempotent な
  明示前提に置き直すこと ([round-2 review L-2] 対応)。
"""
from __future__ import annotations

import time

import pytest

from conftest import (
    MASTODON_DOMAIN,
    SAKURASATO_DOMAIN,
    MastodonClient,
    SakurasatoClient,
    poll_until,
)


# ── 1. Health ────────────────────────────────────────────────


class TestHealth:
    def test_sakurasato_actor_endpoint(self, sakurasato: SakurasatoClient):
        # /healthz が無いので actor JSON で代用 (wait_for_instances 経由でも
        # 200 を確認している、ここは regression のための明示テスト)。
        actor = sakurasato.actor_json("me")
        assert actor["type"] == "Person"
        assert actor["preferredUsername"] == "me"

    def test_sakurasato_whoami_via_local_api(self, sakurasato: SakurasatoClient):
        # UDS 上の Bearer 認証が通り、`/api/v1/whoami` が actor を返すこと。
        who = sakurasato.whoami()
        assert who["preferred_username"] == "me"
        assert who["host"] == SAKURASATO_DOMAIN

    def test_mastodon_instance_endpoint(self, mastodon: MastodonClient):
        creds = mastodon.verify_credentials()
        assert creds["username"] == "bob"

    def test_sakurasato_nodeinfo(self, sakurasato: SakurasatoClient):
        ni = sakurasato.nodeinfo()
        # software 名は `sakurasato` を期待。
        assert ni["software"]["name"] == "sakurasato"


# ── 2. WebFinger ─────────────────────────────────────────────


class TestWebFinger:
    def test_sakurasato_resolves_self(self, sakurasato: SakurasatoClient):
        wf = sakurasato.webfinger(f"me@{SAKURASATO_DOMAIN}")
        assert wf["subject"] == f"acct:me@{SAKURASATO_DOMAIN}"
        # rel="self" link で actor JSON URL が露出すること。
        self_links = [l for l in wf["links"] if l.get("rel") == "self"]
        assert self_links, "WebFinger must expose a self link"
        assert self_links[0]["href"] == f"https://{SAKURASATO_DOMAIN}/users/me"

    def test_mastodon_resolves_self(self, mastodon: MastodonClient):
        wf = mastodon.webfinger(f"bob@{MASTODON_DOMAIN}")
        assert wf["subject"] == f"acct:bob@{MASTODON_DOMAIN}"


# ── 3. Actor ────────────────────────────────────────────────


class TestActor:
    def test_sakurasato_actor_has_keys_and_endpoints(self, sakurasato: SakurasatoClient):
        actor = sakurasato.actor_json("me")
        assert "publicKey" in actor
        pem = actor["publicKey"]["publicKeyPem"]
        assert pem.startswith("-----BEGIN PUBLIC KEY-----")
        for k in ("inbox", "outbox", "followers", "following"):
            assert k in actor, f"actor missing {k}"


# ── 4. Follow ───────────────────────────────────────────────


class TestFollow:
    """両方向の Follow が成立することを副作用で検証する。

    Sakurasato → Mastodon: compose の `sakurasato-prefollow-bob` 一発で
        既に `bob@mastodon` 宛の Follow が `delivery_queue` に乗っている。
        delivery worker が配送し、Mastodon が auto-Accept を返してくる。
    Mastodon → Sakurasato: ここで bob が `accounts/search?resolve=true` →
        `accounts/:id/follow` を実行する。Sakurasato 側は auto-Accept。
    """

    def test_mastodon_resolves_sakurasato_user(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = mastodon.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts, "Mastodon must be able to resolve me@sakurasato via WebFinger"
        assert accounts[0]["username"] == "me"

    def test_mastodon_follows_sakurasato_user(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        # search で actor を読み込ませる (= Mastodon 側に actor row が生まれる)
        accounts = mastodon.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        target_id = accounts[0]["id"]
        result = mastodon.follow(target_id)
        # `following` か `requested` (= 鍵アカ。Sakurasato は今は鍵アカではない)
        # のどちらかが true になる。auto-Accept が走ると最終的に following=True。
        assert result["following"] or result["requested"]

        def is_following() -> bool:
            # Mastodon REST の relationships は最新値を持つ。
            resp = mastodon.http.get(
                "/api/v1/accounts/relationships",
                params={"id[]": target_id},
                headers={"Authorization": f"Bearer {mastodon.token}"},
            )
            resp.raise_for_status()
            rows = resp.json()
            return bool(rows) and rows[0].get("following") is True

        poll_until(is_following, desc="Mastodon shows following=True after auto-Accept")


# ── 5. Note federation (両方向) ─────────────────────────────


class TestNoteFromSakurasato:
    """Sakurasato の投稿が Mastodon の Bob 側 home timeline に届く。"""

    def test_local_note_appears_on_mastodon(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        # 事前条件として TestFollow が走っている (Mastodon Bob が me を follow
        # 済み)。仮に単独実行された場合は念のため follow を張り直す。
        accounts = mastodon.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        mastodon.follow(accounts[0]["id"])

        marker = f"sakurasato-to-mastodon-{int(time.time() * 1000)}"
        sakurasato.create_note(f"hello mastodon: {marker}")

        def appears_on_bob_home() -> bool:
            tl = mastodon.home_timeline(limit=40)
            return any(marker in (s.get("content") or "") for s in tl)

        poll_until(
            appears_on_bob_home,
            desc=f"sakurasato note {marker!r} on Mastodon home timeline",
        )


class TestNoteFromMastodon:
    """Mastodon の投稿が Sakurasato の home timeline に届く (M11 受信実装)。"""

    def test_remote_note_appears_on_sakurasato(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        # sakurasato-prefollow-bob により sakurasato は bob を follow 済み。
        # delivery worker が Follow を投げ、Mastodon が Accept を返した状態。
        # ここで bob が投稿 → sakurasato inbox に Create/Note が届き、
        # follow 経路で home timeline に積まれる。
        marker = f"mastodon-to-sakurasato-{int(time.time() * 1000)}"
        mastodon.create_status(f"hello sakurasato: {marker}")

        def appears_on_me_home() -> bool:
            tl = sakurasato.home_timeline(limit=40)
            return any(marker in (n.get("content") or "") for n in tl)

        poll_until(
            appears_on_me_home,
            desc=f"mastodon note {marker!r} on Sakurasato home timeline",
        )


# ── 6. Reaction federation ─────────────────────────────────


class TestReactionInbound:
    """Mastodon の Favourite (= Like) が Sakurasato 側 reactions に届く。"""

    def test_mastodon_favourite_appears_on_sakurasato(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        # 1) Bob が me を follow 済みであることを保証
        accounts = mastodon.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        mastodon.follow(accounts[0]["id"])

        # 2) sakurasato が投稿
        marker = f"reaction-target-{int(time.time() * 1000)}"
        note = sakurasato.create_note(f"react me: {marker}")
        ap_id = note["ap_id"]

        # 3) Mastodon Bob が当該 status を見つけて favourite する。
        #    Mastodon の URI 検索は `/api/v2/search?q=<uri>&resolve=true`。
        #    Sakurasato Note の canonical URL (= AP id) を渡すと向こうが
        #    status row を作って `statuses[0]` で返す。
        def lookup_status() -> dict | None:
            resp = mastodon.http.get(
                "/api/v2/search",
                params={"q": ap_id, "resolve": "true", "type": "statuses"},
                headers={"Authorization": f"Bearer {mastodon.token}"},
            )
            if resp.status_code != 200:
                return None
            statuses = resp.json().get("statuses") or []
            return statuses[0] if statuses else None

        status = poll_until(lookup_status, desc="Mastodon found sakurasato status by URI")
        fav_resp = mastodon.http.post(
            f"/api/v1/statuses/{status['id']}/favourite",
            headers={"Authorization": f"Bearer {mastodon.token}"},
        )
        fav_resp.raise_for_status()

        # 4) Sakurasato 側 timeline で reactions に Bob 由来の行が立つこと。
        #    Like (content 無し) は `reaction.content = ""` で記録される。
        def has_reaction() -> bool:
            tl = sakurasato.home_timeline(limit=40)
            for n in tl:
                if n.get("ap_id") != ap_id:
                    continue
                for r in n.get("reactions") or []:
                    if r.get("count", 0) >= 1:
                        return True
            return False

        poll_until(
            has_reaction,
            desc=f"sakurasato note {ap_id!r} got at least one reaction",
        )


# ── 7. Move (skipped: 2nd account が必要) ──────────────────


class TestMoveSkip:
    """Move 自動化は 2nd Mastodon account (= 引っ越し先) が要るので skip。

    `compose/federation-test/README.md` の手動手順 (Mastodon の tootctl
    accounts move) は引き続き残し、自動化は別 PR で行う。
    """

    @pytest.mark.skip(reason="Move 自動化は引っ越し先 actor が必要、別 PR で実装")
    def test_move_inbound_reflected_on_sakurasato(self):
        raise AssertionError("not implemented")
