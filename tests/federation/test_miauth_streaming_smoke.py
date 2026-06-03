"""M14 #170 ── MiAuth `/streaming` WebSocket stub の smoke test。

Misskey クライアント (Aria / Milktea / `MissRirica`) は login 後に
`wss://<host>/streaming?i=<token>` を開いてリアルタイム更新を待つ。Sakurasato
は本 endpoint を **最小 stub** で実装 (= `connect` ack + 30s ping/pong、実
イベント送出はしない、`crate::miauth::streaming`)。

## 本 smoke の対象

- **auth gate**: `?i=` なしで接続を試みると 401。`?i=<invalid>` も 401。
- **endpoint 存在**: `/streaming` の path が `/api/*` 系の 404 ではなく
  WebSocket upgrade を試みる側に分岐していること。

## 本 smoke の **非対象**

- 認証通過後の WebSocket 上での `connect` ack シーケンス ── Sakurasato CLI 経由
  でしか MiAuth token を発行できない (= docker socket がない pytest からは
  CLI 起動不可、`test_miauth_flow_parity.py` 流儀)。connect ack 挙動は
  `crate::miauth::streaming::tests` の unit test で覆ってある。
- 本物 Misskey との parity ── Misskey の `/streaming` は AGPL handler 内部
  実装の比重が大きく、wire 互換性検証より「Sakurasato の auth gate が動く」
  ことを smoke で押さえる方が価値が高い。

## AGPL discipline

Misskey の WebSocket frame 仕様は misskey-hub.net の公開仕様から書き起こし。
本テストは observation でしか Misskey に触れず derivative work ではない
(`[[agpl-discipline-miauth]]`).
"""
from __future__ import annotations

import httpx
import pytest

from conftest import (  # noqa: E402
    MISSKEY_ENABLED,
    SAKURASATO_BASE_URL,
    _SSL_VERIFY,
)

pytestmark = pytest.mark.skipif(
    not MISSKEY_ENABLED, reason="MISSKEY_ENABLED=1 でない (= Misskey stack 外)"
)


def _streaming_url() -> str:
    """`/streaming` の WebSocket URL を組み立てる。テスト stack は HTTPS なので
    `wss://`、平文 HTTP テストは想定しない。
    """
    return SAKURASATO_BASE_URL.rstrip("/") + "/streaming"


def test_streaming_without_token_returns_401():
    """`?i=` クエリ無し → WebSocket upgrade 前に 401。"""
    # httpx は WebSocket を直接サポートしないため、`Connection: Upgrade` /
    # `Upgrade: websocket` を明示して `GET /streaming` を叩く。Sakurasato
    # 側は `?i=` 必須なので auth gate で 401 を返すはず (= upgrade ハンドシ
    # ェイクには進まない)。
    headers = {
        "Connection": "Upgrade",
        "Upgrade": "websocket",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
        "Sec-WebSocket-Version": "13",
    }
    resp = httpx.get(
        _streaming_url(),
        headers=headers,
        timeout=10,
        verify=_SSL_VERIFY,
    )
    assert resp.status_code == 401, (
        f"missing token must be 401; got {resp.status_code} body={resp.text[:200]}"
    )


def test_streaming_with_invalid_token_returns_401():
    """`?i=<invalid>` → DB lookup に失敗して 401。"""
    headers = {
        "Connection": "Upgrade",
        "Upgrade": "websocket",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
        "Sec-WebSocket-Version": "13",
    }
    resp = httpx.get(
        _streaming_url(),
        params={"i": "no-such-token-deadbeef"},
        headers=headers,
        timeout=10,
        verify=_SSL_VERIFY,
    )
    assert resp.status_code == 401, (
        f"invalid token must be 401; got {resp.status_code} body={resp.text[:200]}"
    )


def test_streaming_endpoint_is_not_a_404():
    """`/streaming` path 自体は **404 ではない** (= router に登録されている)。

    auth gate に引っかかるので 401 / 403 になるべき。404 / 405 だと router
    に登録されていない / GET 以外しか受け付けていない、のいずれかで、その場合
    MiAuth listener の routing バグ (= #167 / #169 で踏んだ nginx 振り分け
    漏れ系) を疑う材料になる。
    """
    headers = {
        "Connection": "Upgrade",
        "Upgrade": "websocket",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
        "Sec-WebSocket-Version": "13",
    }
    resp = httpx.get(
        _streaming_url(),
        headers=headers,
        timeout=10,
        verify=_SSL_VERIFY,
    )
    assert resp.status_code not in (404, 405), (
        f"/streaming must be routed (not 404/405); got {resp.status_code}"
    )
