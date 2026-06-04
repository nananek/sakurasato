"""M14 #176 ── MiAuth listener `POST /api/endpoints` parity test。

[poppingmoon/aria](https://github.com/poppingmoon/aria) は emoji 取得の前に
`POST /api/endpoints` を叩き、返ってきた配列に `"emojis"` が含まれるかで
`/api/emojis` を使うか `/api/meta.emojis` に fallback するかを判定する
(= `crate::miauth::endpoints` module doc 参照)。Sakurasato が `/api/endpoints`
を返さない / shape を間違えると Aria の emoji picker が空になる。

## 検証範囲

- **wire shape** ── `/api/endpoints` は **top-level JSON array of string**
  (envelope なし)。`misskey-dart` の `Misskey.endpoints()` は `.cast<String>()`
  するので、`{endpoints: [...]}` のような object だと parse 例外になる。
- **side-by-side parity** ── `/api/endpoints` は **認証不要** (= server が何に
  対応するかは公開情報) なので、本物 Misskey と Sakurasato を直叩きして両方が
  flat string array を返すことを確認する。
- **命名 parity** ── Sakurasato が advertise する主要 endpoint 名 (`meta` /
  `i` / `notes/create` / `emojis` 等) が **本物 Misskey の endpoints 一覧にも
  実在する** ことを確認 (= 我々が endpoint 名を捏造していない証拠)。

## AGPL discipline

`/api/endpoints` の shape は misskey-dart (MIT) + api-doc.misskey.io の公開仕様
から書き起こし。本物 Misskey の返り値は本テストで **観察** するだけ
(= observation、derivative work ではない)。Misskey 本体 TypeScript handler は
未参照 ── [[agpl-discipline-miauth]] 準拠。
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

# Aria / Milktea / MissRirica が必ず叩く中核 endpoint。Sakurasato が advertise し、
# かつ本物 Misskey にも実在するはずの名前 (= 命名 parity の照合対象)。
# `emojis` は本 issue (#176) の直接の動機 ── Aria の emoji picker 判定キー。
CORE_ENDPOINTS = (
    "meta",
    "i",
    "emojis",
    "notes/create",
    "notes/show",
    "notes/timeline",
    "users/show",
    "following/create",
)


def _post_list(base_url: str, path: str) -> list:
    """`/api/endpoints` は top-level array を返す。dict ではなく list として受ける。"""
    resp = httpx.post(f"{base_url}{path}", json={}, timeout=10, verify=_SSL_VERIFY)
    assert resp.status_code == 200, (
        f"{base_url}{path} returned {resp.status_code}: {resp.text[:200]}"
    )
    body = resp.json()
    assert isinstance(body, list), (
        f"{base_url}{path} must return a top-level JSON array, got {type(body).__name__}"
    )
    return body


def test_endpoints_is_flat_string_array_on_sakurasato():
    """Sakurasato `/api/endpoints` が **top-level の string 配列** で、空でない。
    envelope (`{endpoints: [...]}`) だと Aria の `.cast<String>()` が落ちる。
    """
    eps = _post_list(SAKURASATO_BASE_URL, "/api/endpoints")
    assert eps, "endpoints list must not be empty"
    non_str = [e for e in eps if not isinstance(e, str)]
    assert not non_str, f"every endpoint entry must be a string; offenders: {non_str!r}"


def test_endpoints_is_flat_string_array_on_misskey():
    """本物 Misskey の `/api/endpoints` も top-level string 配列を返す
    (= 我々の wire shape 判断が正しいことの裏取り)。
    """
    eps = _post_list(MISSKEY_BASE_URL, "/api/endpoints")
    assert eps, "Misskey endpoints list must not be empty"
    non_str = [e for e in eps if not isinstance(e, str)]
    assert not non_str, f"every Misskey endpoint entry must be a string; offenders: {non_str!r}"


def test_endpoints_contains_emojis_on_sakurasato():
    """#176 の核心 ── `emojis` が含まれないと Aria が `/api/meta.emojis` fallback
    (= 我々が emit しない field) に倒れて emoji picker が空になる。
    """
    eps = _post_list(SAKURASATO_BASE_URL, "/api/endpoints")
    assert "emojis" in eps, (
        f"/api/endpoints must contain 'emojis' for Aria's picker; got {sorted(eps)}"
    )


def test_core_endpoints_advertised_by_sakurasato():
    """Sakurasato が中核 endpoint を全部 advertise していること。"""
    eps = set(_post_list(SAKURASATO_BASE_URL, "/api/endpoints"))
    missing = [e for e in CORE_ENDPOINTS if e not in eps]
    assert not missing, f"Sakurasato /api/endpoints missing core endpoints: {missing}"


def test_endpoint_names_exist_on_real_misskey():
    """**命名 parity** ── Sakurasato が advertise する中核 endpoint 名が本物
    Misskey の endpoints 一覧にも実在すること。これが通れば「我々が endpoint 名を
    捏造していない (= `notes/create` であって `createNote` ではない)」が裏付く。
    """
    misskey_eps = set(_post_list(MISSKEY_BASE_URL, "/api/endpoints"))
    not_in_misskey = [e for e in CORE_ENDPOINTS if e not in misskey_eps]
    assert not not_in_misskey, (
        "Sakurasato advertises endpoint names absent from real Misskey "
        f"(naming divergence): {not_in_misskey}"
    )
