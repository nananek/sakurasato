"""Fedibird ↔ Sakurasato programmatic federation tests.

Fedibird は Mastodon のフォークで REST API 互換性が高いため、`test_mastodon.py`
の中核パターン (`MastodonClient` を base_url/domain だけ差し替えて再利用) を
そのまま踏襲する。カバー範囲:

- TestHealth       ── 両 instance の疎通確認
- TestWebFinger    ── 相互の `acct:` 解決
- TestActor        ── Sakurasato actor JSON の鍵/エンドポイント確認
- TestLockedFollow ── **#66**: 鍵アカ状態での Follow pending → approve → Accept
                      round-trip (`test_mastodon.py::TestLockedFollow` 移植)
- TestFollow       ── Sakurasato → Fedibird (`sakurasato-prefollow-bob` 経由) と
                      Fedibird → Sakurasato (`accounts/search?resolve=true` + follow)
- TestNoteFromSakurasato / TestNoteFromFedibird ── 両方向の Note 連合
- TestReactionInbound ── Fedibird Bob の Favourite (= Like) が Sakurasato
                          reactions に反映

Move / Attachment / Visibility matrix 等の重い検証は初回スコープ外 (Mastodon
本家テストで既にカバー済み、fork 固有の差分調査は将来 PR 送り)。

ordering 依存: `TestLockedFollow` は **`TestFollow` より前** に走る必要がある
(既存 accepted follow があると鍵アカ分岐に入らない、`test_mastodon.py` と同じ
制約)。pytest のデフォルト collection 順 (= 定義順) に従う。
"""
from __future__ import annotations

import time

from conftest import (
    FEDIBIRD_DOMAIN,
    SAKURASATO_DOMAIN,
    MastodonClient,
    SakurasatoClient,
    poll_until,
)


# ── 1. Health ────────────────────────────────────────────────


class TestHealth:
    def test_sakurasato_actor_endpoint(self, sakurasato: SakurasatoClient):
        actor = sakurasato.actor_json("me")
        assert actor["type"] == "Person"
        assert actor["preferredUsername"] == "me"

    def test_sakurasato_whoami_via_local_api(self, sakurasato: SakurasatoClient):
        who = sakurasato.whoami()
        assert who["preferred_username"] == "me"
        assert who["host"] == SAKURASATO_DOMAIN

    def test_fedibird_instance_endpoint(self, fedibird: MastodonClient):
        creds = fedibird.verify_credentials()
        assert creds["username"] == "bob"

    def test_sakurasato_nodeinfo(self, sakurasato: SakurasatoClient):
        ni = sakurasato.nodeinfo()
        assert ni["software"]["name"] == "sakurasato"


# ── 2. WebFinger ─────────────────────────────────────────────


class TestWebFinger:
    def test_sakurasato_resolves_self(self, sakurasato: SakurasatoClient):
        wf = sakurasato.webfinger(f"me@{SAKURASATO_DOMAIN}")
        assert wf["subject"] == f"acct:me@{SAKURASATO_DOMAIN}"
        self_links = [l for l in wf["links"] if l.get("rel") == "self"]
        assert self_links, "WebFinger must expose a self link"
        assert self_links[0]["href"] == f"https://{SAKURASATO_DOMAIN}/users/me"

    def test_fedibird_resolves_self(self, fedibird: MastodonClient):
        wf = fedibird.webfinger(f"bob@{FEDIBIRD_DOMAIN}")
        assert wf["subject"] == f"acct:bob@{FEDIBIRD_DOMAIN}"


# ── 3. Actor ────────────────────────────────────────────────


class TestActor:
    def test_sakurasato_actor_has_keys_and_endpoints(self, sakurasato: SakurasatoClient):
        actor = sakurasato.actor_json("me")
        assert "publicKey" in actor
        pem = actor["publicKey"]["publicKeyPem"]
        assert pem.startswith("-----BEGIN PUBLIC KEY-----")
        for k in ("inbox", "outbox", "followers", "following"):
            assert k in actor, f"actor missing {k}"


# ── 3.5. Locked actor (鍵アカ運用 / Issue #66) ──────────────


class TestLockedFollow:
    """`test_mastodon.py::TestLockedFollow` の Fedibird 移植。詳細な設計判断は
    そちらの docstring を参照。要旨: 鍵アカ状態での Follow が pending で
    据え置かれ、CLI approve で Accept が配送され、Fedibird 側 relationship が
    `following=true` に遷移すること。
    """

    @staticmethod
    def _bob_follow_predicate(item: dict) -> bool:
        ap_id = item.get("follower_ap_id") or ""
        return FEDIBIRD_DOMAIN in ap_id and ("bob" in ap_id or "/users/" in ap_id)

    def _reset_bob_follow_row(self, sakurasato: SakurasatoClient) -> None:
        for it in sakurasato.follow_requests(state="all"):
            if self._bob_follow_predicate(it):
                sakurasato.follow_request_delete(it["id"])

    def test_lock_round_trip_with_fedibird_follow(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        self._reset_bob_follow_row(sakurasato)
        accounts = fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts, "Fedibird must resolve me@sakurasato"
        sks_id = accounts[0]["id"]

        try:
            lock_resp = sakurasato.actor_lock()
            assert lock_resp["manually_approves_followers"] is True
            actor = sakurasato.actor_json("me")
            assert actor.get("manuallyApprovesFollowers") is True

            fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
            fedibird.follow(sks_id)

            def list_has_bob() -> int | None:
                for it in sakurasato.follow_requests():
                    if self._bob_follow_predicate(it):
                        return it["id"]
                return None

            follow_id = poll_until(
                list_has_bob,
                desc="follow-request list includes Bob's pending Follow",
            )

            approve_resp = sakurasato.follow_request_approve(follow_id)
            assert approve_resp["new_state"] == "accepted"

            def is_following_now() -> bool:
                resp = fedibird.http.get(
                    "/api/v1/accounts/relationships",
                    params={"id[]": sks_id},
                    headers={"Authorization": f"Bearer {fedibird.token}"},
                )
                if resp.status_code != 200:
                    return False
                rows = resp.json()
                return bool(rows) and rows[0].get("following") is True

            poll_until(
                is_following_now,
                desc="Fedibird relationship.following=true after CLI approve",
            )

            actor_locked = sakurasato.actor_json("me")
            assert actor_locked.get("manuallyApprovesFollowers") is True
        finally:
            try:
                sakurasato.actor_unlock()
            except Exception:  # noqa: BLE001
                pass


# ── 4. Follow ───────────────────────────────────────────────


class TestFollow:
    def test_fedibird_resolves_sakurasato_user(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts, "Fedibird must be able to resolve me@sakurasato via WebFinger"
        assert accounts[0]["username"] == "me"

    def test_fedibird_follows_sakurasato_user(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        target_id = accounts[0]["id"]
        result = fedibird.follow(target_id)
        assert result["following"] or result["requested"]

        def is_following() -> bool:
            resp = fedibird.http.get(
                "/api/v1/accounts/relationships",
                params={"id[]": target_id},
                headers={"Authorization": f"Bearer {fedibird.token}"},
            )
            resp.raise_for_status()
            rows = resp.json()
            return bool(rows) and rows[0].get("following") is True

        poll_until(is_following, desc="Fedibird shows following=True after auto-Accept")


# ── 5. Note federation (両方向) ─────────────────────────────


class TestNoteFromSakurasato:
    def test_local_note_appears_on_fedibird(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        fedibird.follow(accounts[0]["id"])

        marker = f"sakurasato-to-fedibird-{int(time.time() * 1000)}"
        sakurasato.create_note(f"hello fedibird: {marker}")

        def appears_on_bob_home() -> bool:
            tl = fedibird.home_timeline(limit=40)
            return any(marker in (s.get("content") or "") for s in tl)

        poll_until(
            appears_on_bob_home,
            desc=f"sakurasato note {marker!r} on Fedibird home timeline",
        )


class TestNoteFromFedibird:
    def test_remote_note_appears_on_sakurasato(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        marker = f"fedibird-to-sakurasato-{int(time.time() * 1000)}"
        fedibird.create_status(f"hello sakurasato: {marker}")

        def appears_on_me_home() -> bool:
            tl = sakurasato.home_timeline(limit=40)
            return any(marker in (n.get("content") or "") for n in tl)

        poll_until(
            appears_on_me_home,
            desc=f"fedibird note {marker!r} on Sakurasato home timeline",
        )


# ── 6. Reaction federation ─────────────────────────────────


class TestReactionInbound:
    def test_fedibird_favourite_appears_on_sakurasato(
        self, fedibird: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = fedibird.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        fedibird.follow(accounts[0]["id"])

        marker = f"reaction-target-{int(time.time() * 1000)}"
        note = sakurasato.create_note(f"react me: {marker}")
        ap_id = note["ap_id"]

        def lookup_status() -> dict | None:
            resp = fedibird.http.get(
                "/api/v2/search",
                params={"q": ap_id, "resolve": "true", "type": "statuses"},
                headers={"Authorization": f"Bearer {fedibird.token}"},
            )
            if resp.status_code != 200:
                return None
            statuses = resp.json().get("statuses") or []
            return statuses[0] if statuses else None

        status = poll_until(lookup_status, desc="Fedibird found sakurasato status by URI")
        fav_resp = fedibird.http.post(
            f"/api/v1/statuses/{status['id']}/favourite",
            headers={"Authorization": f"Bearer {fedibird.token}"},
        )
        fav_resp.raise_for_status()

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
