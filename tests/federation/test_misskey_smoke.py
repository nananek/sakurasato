"""M14 #162 smoke ── Misskey stack ready 確認 + misskey-py fixture 疎通テスト。

#158 の `test_miauth_flow_parity.py` は本 smoke 完了 (= fixture が正常に
組み立てられる) を前提にしているので、本ファイルが落ちると parity 側の
原因切り分けが難しくなる。Misskey 本体 (= AGPL) は **observed behavior** で
扱い、Sakurasato は変換層 (= MIT) として独立。

## カバレッジ

- `misskey-seed` の admin token がファイル経由で読める
- `/api/i` を misskey-py 経由で叩いて自身の MissUser が返る
- `/api/meta` が標準フィールドを持つ (= host name 確認)
"""
from __future__ import annotations

import os

import pytest

from conftest import MISSKEY_ENABLED  # noqa: E402

pytestmark = pytest.mark.skipif(
    not MISSKEY_ENABLED, reason="MISSKEY_ENABLED=1 でない (= Misskey stack 外)"
)


def test_misskey_token_file_is_present_and_nontrivial(misskey_token: str):
    """`misskey-seed` が token をファイルに書いたことの sanity check。"""
    assert isinstance(misskey_token, str)
    # Misskey の i token は base64url 風の長文字列。10 文字以下は seed エラー。
    assert len(misskey_token) >= 16, f"token too short: len={len(misskey_token)}"


def test_misskey_instance_fixture_reachable(misskey_instance):
    """fixture が httpx Client + meta endpoint を引ける疎通テスト。"""
    resp = misskey_instance.http.post("/api/meta", json={})
    assert resp.status_code == 200, resp.text[:200]
    meta = resp.json()
    # `uri` フィールドが Misskey 仕様で必須 ── 本番 instance の host URI。
    assert "uri" in meta, f"meta missing 'uri': {meta!r}"


def test_misskey_py_client_can_call_i(misskey_py_client):
    """`misskey.Misskey.i()` で自身の MissUser を取れる。

    AGPL-clean check: misskey-py (= MIT, YuzuRyo61) 経由で本物 Misskey に対し
    `/api/i` を叩く ── これは observation であり source の翻訳ではない。
    `username` が seed で作った admin と一致することを確認するだけで充分。
    """
    me = misskey_py_client.i()
    assert isinstance(me, dict)
    expected_user = os.environ.get("MISSKEY_USERNAME", "admin")
    assert me.get("username") == expected_user, (
        f"misskey /api/i returned different user: got {me.get('username')!r} "
        f"expected {expected_user!r}"
    )
    # MissUser 必須フィールドが Misskey 公式に揃っている (= parity test の
    # 前提)。後で parity test が同じキーを Sakurasato 側にも要求する。
    for required in ("id", "username", "host"):
        assert required in me, f"MissUser missing required field {required!r}"
