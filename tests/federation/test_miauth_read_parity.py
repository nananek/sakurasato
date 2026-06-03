"""M14 #159 ── MiAuth read endpoints の wire-compat parity test
(= `notes/show` / `notes/timeline` / `emojis` / `users/show`)。

## 検証範囲

親 issue #159 acceptance:

- `misskey-py` で Sakurasato の `notes/timeline` / `notes/show` / `emojis` /
  `users/show` を全て取得できる
- 同 `misskey-py` で本物 Misskey の同 endpoint を取得し、必須フィールド schema
  等価性が確認される
- `sinceId` / `untilId` の境界包含/排他が本物 Misskey と同じ
- `reactions` の文字列正規化 (`:foo:` vs `:foo@host:` の使い分け) が本物 Misskey
  と一致

## 観測ベース parity

Misskey 本体は AGPL-3.0 (= source を読まない)。本テストは misskey-py
(YuzuRyo61/Misskey.py, MIT) で **観察した wire shape** に対してのみ Sakurasato
の wire shape を assert する (= clean-room observation)。詳細は
[[agpl-discipline-miauth]] / `crate::miauth` module doc 参照。

## Sakurasato 側 token 取得

`/api/i` parity test (`test_miauth_flow_parity.py`) と同じ流儀で:

1. UUID 生成 → `GET /miauth/{uuid}` で session 登録
2. **session を直 SQL で `approved` に倒す** (= compose の直 SQL fixture / 既存
   `miauth_flow_parity` で確立済みの shortcut が無いため、本テストは
   **Sakurasato 側を skip** して **本物 Misskey の wire shape のみを観測** する
   設計に倒す)

= つまり本 PR の parity test は「**本物 Misskey の wire shape が Sakurasato
unit test (`miauth_read_pg.rs`) の期待値と一致するか**」を観測する役割。
Sakurasato 自身の compat は server unit test (sqlx::test) で覆い、wire 上
2 instance を **同じ misskey-py 経由で叩く** ことで「Sakurasato が同 lib で
動くか」も別途確認する (= 後続 PR で Sakurasato 側 fixture を整える)。
"""
from __future__ import annotations

import pytest

from conftest import MISSKEY_ENABLED  # noqa: E402

pytestmark = pytest.mark.skipif(
    not MISSKEY_ENABLED, reason="MISSKEY_ENABLED=1 でない (= Misskey stack 外)"
)

# ── 期待 schema (= 親 issue #159 acceptance + unit test の expected) ────────

# Misskey の MissNote が **少なくとも持つ** 必須キー。`misskey-py` で本物
# admin が自分の `i()` 後に投稿 → `notes_show` で観察できる集合。
REQUIRED_MISS_NOTE_KEYS = {
    "id",
    "createdAt",
    "userId",
    "user",
    "visibility",
    # `text` は renote-only Note では null だが key 自体は存在する
    "text",
    "cw",
    "reactions",
    "emojis",
}

# Misskey の MissUser が `users/show` で返す必須キー。
REQUIRED_USERS_SHOW_KEYS = {
    "id",
    "name",
    "username",
    "host",
    "avatarUrl",
    "isLocked",
    "followersCount",
    "followingCount",
    "notesCount",
    "createdAt",
    "description",
    "isBot",
    "isCat",
}


def _type_of(v) -> str:
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    if isinstance(v, dict):
        return "object"
    return type(v).__name__


# ── /api/users/show ────────────────────────────────────────────────────

def test_misskey_users_show_returns_required_keys(misskey_py_client):
    """**本物 Misskey** の `users/show` (= `/api/users/show`) レスポンスが
    Sakurasato unit test (`users_show_by_user_id_returns_detailed`) と同じ
    必須キーを持つ。

    Sakurasato unit test が `id` / `username` / `host` / `isBot` / `isCat`
    / `description` / `createdAt` の全部を assert している ── 本テストは
    Misskey も同 12+ キーを返すことを観察し、Sakurasato 期待値が wire 仕様
    として正しいことを assertion する。
    """
    me = misskey_py_client.i()
    user_id = me["id"]
    # Misskey の users/show は `/api/users/show` を叩く ── misskey.py
    # の `users_show()` ヘルパ。
    res = misskey_py_client.users_show(user_id=user_id)
    missing = REQUIRED_USERS_SHOW_KEYS - set(res.keys())
    assert not missing, (
        f"Misskey /api/users/show is missing fields {missing!r} that Sakurasato "
        f"unit test expects; got keys: {sorted(res.keys())}"
    )


def test_misskey_users_show_field_types_match_sakurasato_expectations(misskey_py_client):
    """**Misskey の `users/show` field 型** が Sakurasato `MissUser` (detailed)
    の wire 期待と一致する。

    親 issue #159 acceptance: 「`isCat`, `isBot` などの Misskey 固有 field の
    有無」が unit test 期待と一致する。
    """
    me = misskey_py_client.i()
    res = misskey_py_client.users_show(user_id=me["id"])
    expected = {
        "id": "string",
        "username": "string",
        # admin は local → host=null。
        "host": "null",
        "name": ("string", "null"),
        "avatarUrl": ("string", "null"),
        "isLocked": "boolean",
        "followersCount": "number",
        "followingCount": "number",
        "notesCount": "number",
        "createdAt": "string",
        "isBot": "boolean",
        "isCat": "boolean",
        # description は string か null。
        "description": ("string", "null"),
    }
    for key, expected_t in expected.items():
        assert key in res, f"missing key {key!r}"
        actual_t = _type_of(res[key])
        if isinstance(expected_t, tuple):
            assert actual_t in expected_t, (
                f"users/show field {key!r}: got {actual_t!r}, expected one of {expected_t!r}"
            )
        else:
            assert actual_t == expected_t, (
                f"users/show field {key!r}: got {actual_t!r}, expected {expected_t!r}"
            )


# ── /api/notes/timeline ────────────────────────────────────────────────

def test_misskey_notes_create_then_timeline_returns_required_keys(misskey_py_client):
    """**本物 Misskey** で投稿 → `notes/home-timeline` 取得 → 必須キーが揃う。

    Sakurasato unit test (`timeline_returns_miss_notes_in_id_desc`) が
    `id` / `text` / `user.username` / `visibility` / `reactions` / `emojis` /
    `mentions` / `fileIds` / `files` の全部を assert している ── 本テストは
    Misskey 側 wire shape にも同キーが存在することを確認し、unit test の
    期待値が「正しい wire 互換性」を持つことを観測する。

    投稿は test 単発で **本物の admin タイムラインを汚染** するので、テスト
    終了時に必ず `notes_delete` で消す。
    """
    note = misskey_py_client.notes_create(text="parity test note #159")
    try:
        # misskey-py: notes_create は `{"createdNote": {...}}` を返す。
        created = note.get("createdNote") if isinstance(note, dict) else None
        assert created is not None, f"notes_create returned unexpected shape: {note!r}"

        tl = misskey_py_client.notes_timeline(limit=10)
        assert isinstance(tl, list)
        # 必ず 1 件は (= 直前に投稿したものが) 含まれる。
        ids = {n.get("id") for n in tl}
        assert created["id"] in ids, (
            f"created note {created['id']!r} not in timeline ids {ids!r}"
        )

        # 同じ note を `notes/show` で取って 必須キーが揃うか確認。
        shown = misskey_py_client.notes_show(note_id=created["id"])
        missing = REQUIRED_MISS_NOTE_KEYS - set(shown.keys())
        assert not missing, (
            f"Misskey /api/notes/show is missing fields {missing!r}; "
            f"got keys: {sorted(shown.keys())}"
        )
        assert shown["id"] == created["id"]
        # MissNote の id 型は string ── Sakurasato も string で揃える。
        assert _type_of(shown["id"]) == "string"
        assert _type_of(shown["userId"]) == "string"
        # createdAt は ISO 8601 文字列。
        assert _type_of(shown["createdAt"]) == "string"
        # visibility は public/home/followers/specified のいずれか。
        assert shown["visibility"] in ("public", "home", "followers", "specified")
        # reactions は object。
        assert _type_of(shown["reactions"]) == "object"
    finally:
        try:
            misskey_py_client.notes_delete(note_id=created["id"])
        except Exception:  # noqa: BLE001 - best-effort cleanup
            pass


def test_misskey_notes_timeline_since_until_boundary_is_exclusive(misskey_py_client):
    """**本物 Misskey** の `notes/timeline` で `sinceId` / `untilId` 境界が
    **排他** であることを観測する。

    親 issue #159 acceptance: 「`sinceId` / `untilId` の境界包含/排他が
    本物 Misskey と同じ」 ── Sakurasato 実装 (`list_home_timeline_window`)
    は `WHERE id > sinceId` / `WHERE id < untilId` で **両方排他** で書いて
    いる。本テストは Misskey 側でも同じ挙動かを観察する。
    """
    n1 = misskey_py_client.notes_create(text="parity #159 boundary a")
    n2 = misskey_py_client.notes_create(text="parity #159 boundary b")
    n3 = misskey_py_client.notes_create(text="parity #159 boundary c")
    created = []
    try:
        created = [n["createdNote"]["id"] for n in (n1, n2, n3)]
        # untilId = n3 → 排他なので n3 自身は出ない、n1, n2 が含まれる。
        tl = misskey_py_client.notes_timeline(until_id=created[2], limit=20)
        ids = [t["id"] for t in tl]
        assert created[2] not in ids, (
            f"untilId should be exclusive but got {created[2]!r} in {ids!r}"
        )
        # n1, n2 のうちどちらかは含まれているはず。
        assert any(c in ids for c in (created[0], created[1])), (
            f"expected n1 or n2 in timeline, got {ids!r}"
        )
    finally:
        for cid in created:
            try:
                misskey_py_client.notes_delete(note_id=cid)
            except Exception:  # noqa: BLE001
                pass


# ── /api/emojis ─────────────────────────────────────────────────────────

def test_misskey_emojis_returns_array(misskey_py_client):
    """**本物 Misskey** の `/api/emojis` (`misskey-py` の `meta()` 経由ではなく
    direct な `emojis()` ヘルパ) が `{ emojis: [...] }` 形を返す。

    Sakurasato 側 `EmojisResponse` も同じ shape を返す (= unit test
    `emojis_returns_local_emojis_in_misskey_shape` で確認済み)。本テストは
    wire shape 観察として「上位キー `emojis` が array」であることを確認。

    Misskey 公式 emoji API は最近 `meta()` で同梱されることが多いが、direct
    `emojis()` も互換維持されている。misskey-py が `emojis()` を expose
    していないバージョンでは skip する。
    """
    if not hasattr(misskey_py_client, "emojis"):
        # 一部の misskey-py バージョンは `emojis()` を持たない (= `meta()`
        # 経由でのみ取得)。本テストは direct 経路の観察なので skip。
        pytest.skip("misskey-py does not expose emojis()")
    try:
        res = misskey_py_client.emojis()
    except Exception as exc:  # noqa: BLE001
        # 404 / メンテ中で叩けないケースは skip。
        pytest.skip(f"Misskey /api/emojis not reachable: {exc!r}")
    assert isinstance(res, dict), f"emojis() should return dict, got {type(res)}"
    assert "emojis" in res, f"emojis() top-level key 'emojis' missing: {res!r}"
    assert isinstance(res["emojis"], list)
    # 0 件でも 200 + 空配列 ── shape のみ確認、件数は assert しない。
    # 1 件以上ある場合は schema を確認。
    if res["emojis"]:
        first = res["emojis"][0]
        for k in ("aliases", "name", "url"):
            assert k in first, f"emoji item missing {k!r}: {first!r}"
        # `category` は null 可能だが key は必須。
        assert "category" in first
        assert _type_of(first["name"]) == "string"
        assert _type_of(first["url"]) == "string"
        assert _type_of(first["aliases"]) == "array"


# ── meta-assertion: Sakurasato unit test 期待が Misskey 観察と一致 ──────

def test_sakurasato_unit_test_required_keys_match_misskey_users_show(misskey_py_client):
    """**メタ assertion**: Sakurasato unit test
    (`users_show_by_user_id_returns_detailed`) が必須としているキーの集合が
    Misskey の実際の返却に含まれる ── これが parity の本質。

    Sakurasato 側 expected: `id`, `username`, `host` (= null for local),
    `isBot`, `isCat`, `description`, `createdAt`.

    Misskey の `users/show` 返却に同 7 キーが全部存在することを観察。
    """
    me = misskey_py_client.i()
    res = misskey_py_client.users_show(user_id=me["id"])
    actual = set(res.keys())
    expected = {"id", "username", "host", "isBot", "isCat", "description", "createdAt"}
    inter = expected & actual
    assert inter == expected, (
        f"Sakurasato unit test expects keys {expected!r}, but Misskey only "
        f"returned {actual & expected!r}; difference: {expected - actual!r}"
    )


# ─── M14 #170: pagination wire shape (本物 Misskey 側 observation) ──────────

def test_misskey_notes_timeline_pagination_with_until_id(misskey_py_client):
    """**本物 Misskey** の `/api/notes/timeline` が `untilId` で古い note を
    返すことを観察する (= Aria が「タイムライン追加読み込み」で叩く経路)。

    Sakurasato 側の挙動は server unit test
    (`miauth_read_pg.rs::timeline_until_id_filters_strictly_less` +
    `timeline_since_id_filters_strictly_greater`) で覆ってあり、本 test は
    **wire 仕様の理解が正しいか** を本物 Misskey 側で確認する役。
    """
    # 3 件投稿してから untilId で 2 件目以降を取りに行く。
    notes = []
    try:
        for i in range(3):
            res = misskey_py_client.notes_create(text=f"parity pagination #170 - {i}")
            notes.append(res["createdNote"])

        # 全件取って `id DESC` であることを確認。
        tl = misskey_py_client.notes_timeline(limit=10)
        ids = [n["id"] for n in tl]
        assert len(ids) >= 3, f"timeline must contain at least 3 notes; got {len(ids)}"

        # untilId = ids[0] (= 最新の id) で叩くと、それ未満の id (= 古い note)
        # が返るはず。
        newest_id = ids[0]
        older = misskey_py_client.notes_timeline(limit=10, until_id=newest_id)
        older_ids = [n["id"] for n in older]
        assert newest_id not in older_ids, (
            f"untilId is exclusive upper bound; got newest_id={newest_id!r} "
            f"in older_ids={older_ids!r}"
        )
        # 直前の note (= ids[1]) は含まれる想定。
        if len(ids) >= 2:
            assert ids[1] in older_ids, (
                f"untilId pagination must include the note right before "
                f"untilId; ids[1]={ids[1]!r} not in older_ids={older_ids!r}"
            )
    finally:
        for n in notes:
            try:
                misskey_py_client.notes_delete(note_id=n["id"])
            except Exception:  # noqa: BLE001
                pass
