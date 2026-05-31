"""Mastodon ↔ Sakurasato programmatic federation tests.

カバー範囲 (Issue #56 + 後続 issues):

- TestHealth                       ── 両 instance の `/healthz` / NodeInfo
- TestWebFinger                    ── 相互の `acct:` 解決
- TestActor                        ── 相互の actor JSON 取得
- TestReplyDeliveryToNonFollower   ── **#64**: 未フォロー相手への返信が届く
- TestFollow                       ── Sakurasato → Mastodon (`sakurasato-prefollow-bob` 経由) と
                                      Mastodon → Sakurasato (`accounts/search?resolve=true` + follow)
- TestNoteFromSks                  ── Sakurasato 投稿 → Mastodon Bob の home timeline に届く
- TestNoteFromMastodon             ── Mastodon 投稿 → Sakurasato Me の home timeline に届く (= #55 で実装)
- TestReactionInbound              ── Mastodon Bob の Favourite (= Like) が Sakurasato 側 reactions に反映
- TestMoveSkip                     ── alsoKnownAs + Move は 2nd Mastodon account が要るため将来 PR で

ポイント:

- Sakurasato 側操作は **すべて Bearer 認証付きの UDS local API** 経由。公開 TCP
  には載っていない `/api/v1/*` を本来の経路で触る。
- Mastodon 側操作は OAuth password grant でログインした `bob` で行う。
- federation は非同期 (delivery worker + Sidekiq) なので、`poll_until` で最大
  60 秒待つ。
- 連合経路は **side-effect で観測** する (= Bob が home_timeline に load し直して
  確認する等)。直接 DB を覗かない。

ordering 依存 (重要):

- `TestReplyDeliveryToNonFollower` は **`TestFollow` より前** に走らなければ
  ならない (Bob → Sakurasato follow が成立すると followers loop で reply が
  届いてしまい、未フォロー経路の分離検証ができなくなる)。
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


# ── 3.5. Reply delivery to non-follower (#64) ─────────────


class TestReplyDeliveryToNonFollower:
    """**#64**: Sakurasato が Bob の note に返信したとき、Bob が Sakurasato を
    follow していなくても、Bob の inbox に reply が届くこと。

    バグ修正前: `enqueue_to_followers` だけが配送経路だったため、Bob が
    Sakurasato の follower でない場合は reply の配送先が空になり、Bob は
    気付けなかった。修正後: 親 author URI が `cc` に乗り、Bob の inbox が
    `delivery_queue` に積まれる。

    本テストは **TestFollow より前** に実行することが重要 ── TestFollow で
    Bob → Sakurasato follow が成立すると followers loop でも届くようになり、
    バグの分離検証ができなくなる。pytest の default 順 (= 定義順) で先に
    走るようにこの位置に置いている (test_mastodon.py 冒頭の ordering 注記)。
    """

    def test_reply_to_non_follower_reaches_them(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        # **#64 F-5 (テスト前提の明示化)**: 本テストは「Sakurasato が Bob を
        # follow している (= sakurasato-prefollow-bob 経由)」が前提。前提が
        # 崩れると Phase 2 の poll_until が "seed not found" でタイムアウトし
        # ミスリーディングになるので、まず Bob から見た sakurasato の relationship
        # を確認する (= sakurasato → Bob の Follow が auto-Accept された証拠は
        # Bob 側の `followed_by=True`)。
        bob_accounts = mastodon.search_accounts(
            f"me@{SAKURASATO_DOMAIN}", resolve=True
        )
        assert bob_accounts, "Mastodon must be able to resolve me@sakurasato"
        sks_id = bob_accounts[0]["id"]

        def sakurasato_follows_bob_per_mastodon() -> bool:
            resp = mastodon.http.get(
                "/api/v1/accounts/relationships",
                params={"id[]": sks_id},
                headers={"Authorization": f"Bearer {mastodon.token}"},
            )
            if resp.status_code != 200:
                return False
            rows = resp.json()
            return bool(rows) and rows[0].get("followed_by") is True

        poll_until(
            sakurasato_follows_bob_per_mastodon,
            desc="sakurasato-prefollow-bob must have completed; "
            "Mastodon should report followed_by=True",
        )

        # Phase 1: Bob が seed status を投稿。
        seed_marker = f"reply-seed-{int(time.time() * 1000)}"
        bob_status = mastodon.create_status(f"seed: {seed_marker}")
        bob_status_id = bob_status["id"]

        # Phase 2: Sakurasato (= Bob を follow 済み) が ingest するのを待ち、
        # ap_id を拾う。
        def sakurasato_has_seed() -> str | None:
            tl = sakurasato.home_timeline(limit=40)
            for n in tl:
                if seed_marker in (n.get("content") or ""):
                    return n.get("ap_id")
            return None

        seed_ap_id = poll_until(
            sakurasato_has_seed,
            desc=f"sakurasato ingested seed {seed_marker}",
        )

        # Phase 3: Sakurasato が seed に reply する。Bob はまだ Sakurasato を
        # follow していない状態で投げるのがポイント。
        reply_marker = f"reply-body-{int(time.time() * 1000)}"
        sakurasato.create_note(
            f"reply: {reply_marker}",
            in_reply_to_ap_id=seed_ap_id,
        )

        # Phase 4: Mastodon 側の status context に reply が descendants として
        # 出現するまで待つ。Mastodon は inbox 受領で status row を作る ──
        # follow 関係に関わらず、cc / 親 author 経路で届けば context に乗る。
        def reply_visible_on_mastodon() -> bool:
            resp = mastodon.http.get(
                f"/api/v1/statuses/{bob_status_id}/context",
                headers={"Authorization": f"Bearer {mastodon.token}"},
            )
            if resp.status_code != 200:
                return False
            ctx = resp.json()
            descendants = ctx.get("descendants") or []
            return any(reply_marker in (s.get("content") or "") for s in descendants)

        poll_until(
            reply_visible_on_mastodon,
            desc=f"reply {reply_marker} visible on Mastodon as descendant of {bob_status_id}",
        )


# ── 3.6. Locked actor (鍵アカ運用 / Issue #66) ──────────────


class TestLockedFollow:
    """**#66**: Sakurasato が `manuallyApprovesFollowers = true` の鍵アカ状態
    のとき、Mastodon Bob からの Follow が `requested` で据え置かれ、Sakurasato
    の `follow-request approve` で initial に Accept が配送されて Mastodon の
    relationship が `following = true` に遷移すること。

    本テストは **TestFollow より前** に走る必要がある:

    - Bob はまだ Sakurasato を follow していない (= TestFollow の前提) 状態で
      開始することで、鍵アカ分岐 (= 新規 Follow が pending で据え置かれる)
      を確実に通せる。
    - 既存 accepted follow が存在すると `dispatch::handler::handle_follow` が
      Accept 再送ブランチに入って locked 判定をスキップする (= 設計どおり)
      ため、テストにならない。

    終了時に Sakurasato を unlock + Bob は Sakurasato を follow 済み状態で
    終わるので、後続の TestFollow / TestNoteFromSakurasato 等は **そのまま**
    走る (idempotent follow + 既に accepted)。
    """

    def test_lock_round_trip_with_mastodon_follow(
        self, mastodon: MastodonClient, sakurasato: SakurasatoClient
    ):
        try:
            # 1) Sakurasato を lock。actor JSON で manuallyApprovesFollowers=true。
            lock_resp = sakurasato.actor_lock()
            assert lock_resp["manually_approves_followers"] is True
            actor = sakurasato.actor_json("me")
            assert actor.get("manuallyApprovesFollowers") is True

            # 2) Bob が search → follow。Sakurasato 側 dispatch は pending で
            #    据え置く ので Mastodon は requested=true を観測する。
            #    Mastodon は actor JSON の locked 判定を信用するため、UI 上は
            #    follow Button 押下直後から `requested=true` で返る。
            accounts = mastodon.search_accounts(
                f"me@{SAKURASATO_DOMAIN}", resolve=True
            )
            assert accounts
            sks_id = accounts[0]["id"]
            follow_resp = mastodon.follow(sks_id)
            # follow_resp は relationship object。`requested` または
            # `following` のどちらかが True。Mastodon は actor JSON 再取得を
            # 含むので、初回 follow 直後は requested=True、following=False
            # を期待。
            assert follow_resp["requested"] or follow_resp["following"]

            # 3) Sakurasato の pending リストに 1 件出現するまで待つ
            #    (delivery / inbox 処理が非同期)。
            def list_has_bob() -> int | None:
                items = sakurasato.follow_requests()
                for it in items:
                    if it["follower_ap_id"].endswith("/users/bob") or (
                        "bob" in it["follower_ap_id"]
                        and MASTODON_DOMAIN in it["follower_ap_id"]
                    ):
                        return it["id"]
                return None

            follow_id = poll_until(
                list_has_bob,
                desc="follow-request list includes Bob's pending Follow",
            )

            # 4) approve → Sakurasato が Accept を Mastodon に配送。
            approve_resp = sakurasato.follow_request_approve(follow_id)
            assert approve_resp["new_state"] == "accepted"

            # 5) Mastodon 側 relationship が following=true に遷移するまで待つ。
            def is_following_now() -> bool:
                resp = mastodon.http.get(
                    "/api/v1/accounts/relationships",
                    params={"id[]": sks_id},
                    headers={"Authorization": f"Bearer {mastodon.token}"},
                )
                if resp.status_code != 200:
                    return False
                rows = resp.json()
                return bool(rows) and rows[0].get("following") is True

            poll_until(
                is_following_now,
                desc="Mastodon relationship.following=true after CLI approve",
            )
        finally:
            # 後続テストの前提 (= 鍵アカ off) に戻す。lock した状態で
            # 例外発生 / abort しても unlock を必ず通すために finally。
            sakurasato.actor_unlock()
            actor_after = sakurasato.actor_json("me")
            assert actor_after.get("manuallyApprovesFollowers") is False


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
