"""M14 #168 ── MiAuth listener `/api/meta` + nodeinfo + `/api/stats` parity test。

Sakurasato MiAuth listener が Misskey クライアント (Milktea / MissRirica) の
**login probe** に正しく応答するかを、本物 Misskey と並べて検証する。

## 検証範囲

Misskey クライアントは server URL を入力した瞬間に以下を probe する:

1. `POST /api/meta` ── instance 情報。**無いと login UI が出ない**
2. `GET /.well-known/nodeinfo` → `GET /nodeinfo/2.1` ── server 種別判定 (一部 client)
3. `POST /api/stats` ── instance 統計 (overview UI)

本テストは:

- **schema 等価性** ── 本物 Misskey 側 (= `misskey/misskey:latest`) と
  Sakurasato MiAuth listener の両方を直叩きし、`/api/meta` の **必須キー** が
  両方で揃うことを検査する (= 値は別 instance なので差異あり、型タグだけ比較)
- **`features.miauth: true`** ── client が「MiAuth 経路で login しよう」と
  判定する flag。Sakurasato 側で必ず true で乗っていることを確認
- **NodeInfo 2.1** ── `software.name = "sakurasato"` (= 偽装しない、accurate
  に名乗る) と `protocols` に `"activitypub"` が含まれることを確認
- **`/api/stats`** ── camelCase keys と整数型 が揃うことを確認

## AGPL discipline

misskey-hub.net / api-doc.misskey.io の公開仕様と nkv-proxy の reference を
基に **必須キーの最小集合** を Sakurasato 側で書き起こした。本物 Misskey の
**観察結果** は本テストで取り出すだけ (= observation、derivative work ではない)。
Misskey 本体 TypeScript handler は未参照 ── `[[agpl-discipline-miauth]]` 準拠。
"""
from __future__ import annotations

import httpx
import pytest

from conftest import (  # noqa: E402
    MISSKEY_BASE_URL,
    MISSKEY_ENABLED,
    SAKURASATO_BASE_URL,
    _SSL_VERIFY,
)

pytestmark = pytest.mark.skipif(
    not MISSKEY_ENABLED, reason="MISSKEY_ENABLED=1 でない (= Misskey stack 外)"
)

# /api/meta の最小必須キー (= Misskey クライアントが login UI 構成で touch する).
# nkv-proxy / api-doc.misskey.io / misskey-hub.net から書き起こし。値ではなく
# 存在 + 型タグのみ照合する。
REQUIRED_META_KEYS = (
    "name",
    "version",
    "uri",
    "description",
    "maintainerName",
    "maintainerEmail",
    "repositoryUrl",
    "feedbackUrl",
    "disableRegistration",
    "emailRequiredForSignup",
    "enableHcaptcha",
    "enableRecaptcha",
    "enableTurnstile",
    "maxNoteTextLength",
    "serverRules",
    "policies",
    "features",
)


def _type_of(v) -> str:
    """JSON value → 型タグ。`null` は別タグ。`int` / `float` を `number` で
    統合 (= Misskey/JS 側は number 単一型のため)。
    """
    if v is None:
        return "null"
    if isinstance(v, bool):
        # bool は int のサブクラスなので **bool を先に判定**。
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    if isinstance(v, dict):
        return "object"
    raise AssertionError(f"unknown JSON type for value: {v!r}")


def _post_json(base_url: str, path: str, body: dict | None = None) -> dict:
    body = body or {}
    resp = httpx.post(
        f"{base_url}{path}",
        json=body,
        timeout=10,
        verify=_SSL_VERIFY,
    )
    assert resp.status_code == 200, (
        f"{base_url}{path} returned {resp.status_code}: {resp.text[:200]}"
    )
    return resp.json()


def _get_json(base_url: str, path: str) -> dict:
    resp = httpx.get(
        f"{base_url}{path}",
        timeout=10,
        verify=_SSL_VERIFY,
    )
    assert resp.status_code == 200, (
        f"{base_url}{path} returned {resp.status_code}: {resp.text[:200]}"
    )
    return resp.json()


def test_meta_has_all_required_keys_on_sakurasato():
    """Sakurasato MiAuth listener の /api/meta が必須キーを全部返すこと。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/meta")
    missing = [k for k in REQUIRED_META_KEYS if k not in sks]
    assert not missing, f"Sakurasato /api/meta missing keys: {missing}"


def test_meta_has_all_required_keys_on_misskey():
    """本物 Misskey でも同じ必須キーが返ること (= 我々の必須リストが過剰
    でないことの確認)。"""
    misskey = _post_json(MISSKEY_BASE_URL, "/api/meta")
    missing = [k for k in REQUIRED_META_KEYS if k not in misskey]
    assert not missing, f"Misskey /api/meta missing keys: {missing}"


def test_meta_type_parity():
    """両 instance で /api/meta の必須キーの **JSON 型** が一致すること。
    値は別 instance なので差異あり、型タグだけ比較する。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/meta")
    misskey = _post_json(MISSKEY_BASE_URL, "/api/meta")
    mismatches = []
    for key in REQUIRED_META_KEYS:
        if key not in sks or key not in misskey:
            continue  # 別 test で検出済み
        sks_t = _type_of(sks[key])
        mk_t = _type_of(misskey[key])
        # Misskey 側で null の field は description / maintainerName 等。
        # Sakurasato 側も null で OK。逆も同じ。null は許容差分とする。
        if "null" in (sks_t, mk_t):
            continue
        if sks_t != mk_t:
            mismatches.append(f"{key}: sakurasato={sks_t} misskey={mk_t}")
    assert not mismatches, "type mismatches in /api/meta: " + "; ".join(mismatches)


def test_meta_features_miauth_is_true_on_sakurasato():
    """Sakurasato 側 `features.miauth: true` (= client が MiAuth login を試す flag)。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/meta")
    features = sks.get("features", {})
    assert features.get("miauth") is True, (
        f"features.miauth must be True on Sakurasato; got {features}"
    )


def test_meta_disable_registration_on_sakurasato():
    """お一人様前提 ── 登録は disabled 固定。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/meta")
    assert sks.get("disableRegistration") is True
    assert sks.get("enableHcaptcha") is False
    assert sks.get("enableRecaptcha") is False
    assert sks.get("enableTurnstile") is False
    assert sks.get("enableEmail") is False


def test_meta_max_note_text_length_on_sakurasato():
    """`maxNoteTextLength` は client が compose UI の文字数制限に使う。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/meta")
    n = sks.get("maxNoteTextLength")
    assert isinstance(n, int) and n > 0, (
        f"maxNoteTextLength must be a positive int; got {n!r}"
    )


def test_nodeinfo_discovery_on_sakurasato():
    """`/.well-known/nodeinfo` が discovery を返し、links[0].rel が 2.1 schema。"""
    body = _get_json(SAKURASATO_BASE_URL, "/.well-known/nodeinfo")
    links = body.get("links", [])
    assert links, "discovery must contain at least one link"
    rels = {link.get("rel") for link in links}
    assert "http://nodeinfo.diaspora.software/ns/schema/2.1" in rels, (
        f"2.1 schema rel not found; got {rels}"
    )


def test_nodeinfo_v2_1_on_sakurasato():
    """`/nodeinfo/2.1` の `software.name` が `"sakurasato"` (= 偽装しない)
    で、`protocols` に `"activitypub"` が含まれる。
    """
    body = _get_json(SAKURASATO_BASE_URL, "/nodeinfo/2.1")
    assert body.get("version") == "2.1"
    assert body.get("software", {}).get("name") == "sakurasato"
    protocols = body.get("protocols", [])
    assert "activitypub" in protocols, f"protocols must include activitypub; got {protocols}"


def test_stats_returns_required_camelcase_keys_on_sakurasato():
    """`/api/stats` のキーが Misskey 仕様の camelCase (notesCount 等) と一致。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/stats")
    required = (
        "notesCount",
        "originalNotesCount",
        "usersCount",
        "originalUsersCount",
        "instances",
        "driveUsageLocal",
        "driveUsageRemote",
    )
    missing = [k for k in required if k not in sks]
    assert not missing, f"Sakurasato /api/stats missing keys: {missing}"
    # すべて非負整数。
    for k in required:
        v = sks[k]
        assert isinstance(v, int) and v >= 0, (
            f"{k} must be non-negative int; got {v!r}"
        )


def test_stats_type_parity():
    """Misskey と Sakurasato で /api/stats の各キーが number 型で揃うこと。"""
    sks = _post_json(SAKURASATO_BASE_URL, "/api/stats")
    misskey = _post_json(MISSKEY_BASE_URL, "/api/stats")
    common_keys = set(sks.keys()) & set(misskey.keys())
    # 少なくとも notesCount + usersCount は両方に存在する想定。
    assert "notesCount" in common_keys
    assert "usersCount" in common_keys
    for k in common_keys:
        sks_t = _type_of(sks[k])
        mk_t = _type_of(misskey[k])
        if "null" in (sks_t, mk_t):
            continue
        assert sks_t == mk_t, (
            f"/api/stats[{k}] type mismatch: sakurasato={sks_t} misskey={mk_t}"
        )
