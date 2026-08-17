"""Mitra ↔ Sakurasato programmatic federation tests.

Mitra は Mastodon 互換 API のサブセットを実装する Rust 製 ActivityPub サーバ
(FEP-521a Multikey 対応、RFC 9421 + Ed25519 の対応能力を持つ)。本スイートは
`tmp/plan-follow-request-accept-mitra.md` (2026-08-15 実施の実機調査) で確認
済みの手動 e2e フローを programmatic 化したもの。

**署名方式に関する注記**: 実機調査により、Sakurasato → Mitra 方向の受信
Follow は Sakurasato の cavage RSA-SHA256 署名で成立することを確認済み
(Mitra の署名検証は cavage/RFC9421 両対応で、cavage から先に試行される)。
RFC 9421 + Ed25519 の受信側検証は本スイートではなく
`crates/server/tests/dispatch_pg.rs` / `inbox_signature_tests.rs` で
Mitra 非依存にカバーする (README の「RFC 9421 + Ed25519」は Mitra の対応
*能力* の話であり、実通信で観測された署名方式ではない)。

カバー範囲:

- TestHealth       ── 両 instance の疎通確認
- TestWebFinger    ── 相互の `acct:` 解決
- TestActor        ── Sakurasato actor JSON の鍵/エンドポイント確認
- TestLockedFollow ── **中核**: 鍵アカ状態で Mitra bob → me の Follow が
                      pending → approve → Accept → Mitra 側
                      `relationship.following=true` (実機調査 §1 の再現)
- TestFollow       ── 非 lock 状態での双方向 Follow
                      (Mitra→Sakurasato / Sakurasato→Mitra)
- TestNoteFromSakurasato / TestNoteFromMitra ── 両方向の Note 連合

ordering 依存: `TestLockedFollow` は **`TestFollow` より前** に走る必要が
ある (既存 accepted follow があると鍵アカ分岐に入らない、
`test_mastodon.py::TestLockedFollow` と同じ制約)。
"""
from __future__ import annotations

import time

from conftest import (
    MITRA_DOMAIN,
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

    def test_mitra_verify_credentials(self, mitra: MastodonClient):
        creds = mitra.verify_credentials()
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

    def test_mitra_resolves_self(self, mitra: MastodonClient):
        wf = mitra.webfinger(f"bob@{MITRA_DOMAIN}")
        assert wf["subject"] == f"acct:bob@{MITRA_DOMAIN}"


# ── 3. Actor ────────────────────────────────────────────────


class TestActor:
    def test_sakurasato_actor_has_keys_and_endpoints(self, sakurasato: SakurasatoClient):
        actor = sakurasato.actor_json("me")
        assert "publicKey" in actor
        pem = actor["publicKey"]["publicKeyPem"]
        assert pem.startswith("-----BEGIN PUBLIC KEY-----")
        for k in ("inbox", "outbox", "followers", "following"):
            assert k in actor, f"actor missing {k}"


# ── 3.5. Locked actor round-trip (実機調査 §1 の再現) ────────


class TestLockedFollow:
    """`tmp/plan-follow-request-accept-mitra.md` §1 の手動 e2e (全ステップ
    成功確認済み) を programmatic 化したもの。詳細な設計判断は
    `test_mastodon.py::TestLockedFollow` の docstring を参照。
    """

    @staticmethod
    def _bob_follow_predicate(item: dict) -> bool:
        ap_id = item.get("follower_ap_id") or ""
        return MITRA_DOMAIN in ap_id and ("bob" in ap_id or "/users/" in ap_id)

    def _reset_bob_follow_row(self, sakurasato: SakurasatoClient) -> None:
        for it in sakurasato.follow_requests(state="all"):
            if self._bob_follow_predicate(it):
                sakurasato.follow_request_delete(it["id"])

    def test_lock_round_trip_with_mitra_follow(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        self._reset_bob_follow_row(sakurasato)
        accounts = mitra.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts, "Mitra must resolve me@sakurasato"
        sks_id = accounts[0]["id"]

        try:
            lock_resp = sakurasato.actor_lock()
            assert lock_resp["manually_approves_followers"] is True
            actor = sakurasato.actor_json("me")
            assert actor.get("manuallyApprovesFollowers") is True

            # Mitra 側に locked=true を再認識させてから follow (実機調査 §1
            # 手順 2-3 の再現)。
            mitra.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
            mitra.follow(sks_id)

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
                resp = mitra.http.get(
                    "/api/v1/accounts/relationships",
                    params={"id[]": sks_id},
                    headers={"Authorization": f"Bearer {mitra.token}"},
                )
                if resp.status_code != 200:
                    return False
                rows = resp.json()
                return bool(rows) and rows[0].get("following") is True

            poll_until(
                is_following_now,
                desc="Mitra relationship.following=true after CLI approve",
            )

            actor_locked = sakurasato.actor_json("me")
            assert actor_locked.get("manuallyApprovesFollowers") is True
        finally:
            try:
                sakurasato.actor_unlock()
            except Exception:  # noqa: BLE001
                pass


# ── 4. Follow (非 lock 状態、双方向) ─────────────────────────


class TestFollow:
    def test_mitra_resolves_sakurasato_user(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = mitra.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts, "Mitra must be able to resolve me@sakurasato via WebFinger"
        assert accounts[0]["username"] == "me"

    def test_mitra_follows_sakurasato_user(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        accounts = mitra.search_accounts(f"me@{SAKURASATO_DOMAIN}", resolve=True)
        assert accounts
        target_id = accounts[0]["id"]
        mitra.follow(target_id)

        def is_following() -> bool:
            resp = mitra.http.get(
                "/api/v1/accounts/relationships",
                params={"id[]": target_id},
                headers={"Authorization": f"Bearer {mitra.token}"},
            )
            resp.raise_for_status()
            rows = resp.json()
            return bool(rows) and rows[0].get("following") is True

        poll_until(is_following, desc="Mitra shows following=True after auto-Accept")

    def test_sakurasato_follows_mitra_user(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        # local API `/api/v1/follow` 経由。WebFinger は media-proxy 経由で解決
        # される (M10 PR #51)。
        result = sakurasato.follow(f"bob@{MITRA_DOMAIN}")
        assert result["state"] in ("pending", "accepted")

        def is_accepted() -> bool:
            for entry in sakurasato.following():
                actor = entry.get("actor") or {}
                ap_id = actor.get("ap_id") or ""
                if MITRA_DOMAIN in ap_id and (
                    "bob" in ap_id or "/users/" in ap_id
                ):
                    return entry.get("follow_state") == "accepted"
            return False

        poll_until(
            is_accepted,
            desc="sakurasato following list shows bob@mitra as accepted",
        )


# ── 5. Note federation (両方向) ─────────────────────────────


class TestNoteFromSakurasato:
    def test_local_note_appears_on_mitra(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        marker = f"sakurasato-to-mitra-{int(time.time() * 1000)}"
        sakurasato.create_note(f"hello mitra: {marker}")

        def appears_on_bob_home() -> bool:
            tl = mitra.home_timeline(limit=40)
            return any(marker in (s.get("content") or "") for s in tl)

        poll_until(
            appears_on_bob_home,
            desc=f"sakurasato note {marker!r} on Mitra home timeline",
        )


class TestNoteFromMitra:
    def test_remote_note_appears_on_sakurasato(
        self, mitra: MastodonClient, sakurasato: SakurasatoClient
    ):
        marker = f"mitra-to-sakurasato-{int(time.time() * 1000)}"
        mitra.create_status(f"hello sakurasato: {marker}")

        def appears_on_me_home() -> bool:
            tl = sakurasato.home_timeline(limit=40)
            return any(marker in (n.get("content") or "") for n in tl)

        poll_until(
            appears_on_me_home,
            desc=f"mitra note {marker!r} on Sakurasato home timeline",
        )
