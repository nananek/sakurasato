"""Fixtures and helpers for sakurasato federation tests.

Sakurasato 側は **UDS (Unix domain socket) 経由** の Bearer トークン認証付き
ローカル API を叩く ── 公開 TCP には載っていない `/api/v1/*` を本来の経路で
触るため。Mastodon 側は通常の HTTPS OAuth で叩く。

外部要件:

- compose 側で `sakurasato_local_api` 名前付き volume を /home/nonroot に
  マウントしておくこと (= UDS が pytest コンテナから見える)
- `sakurasato-token-issuer` が `/tokens/pytest.token` に raw token を書いた状態
- `mastodon-web` が healthy で `bob`/`Password1234!` が作成済み
- 共有 test CA が `/certs/ca.crt` にあり、`SSL_CERT_FILE` で reqwest/httpx に
  反映されている (= mastodon -> sakurasato の TLS 検証が通る)
"""
from __future__ import annotations

import os
import ssl
import time
from pathlib import Path
from typing import Any, Callable

import httpx
import pytest

SAKURASATO_BASE_URL = os.environ.get("SAKURASATO_BASE_URL", "https://sakurasato")
SAKURASATO_DOMAIN = os.environ.get("SAKURASATO_DOMAIN", "sakurasato")
SAKURASATO_LOCAL_API_SOCKET = os.environ.get(
    "SAKURASATO_LOCAL_API_SOCKET", "/home/nonroot/local.sock"
)
SAKURASATO_TOKEN_FILE = os.environ.get(
    "SAKURASATO_TOKEN_FILE", "/tokens/pytest.token"
)

MASTODON_BASE_URL = os.environ.get("MASTODON_BASE_URL", "https://mastodon")
MASTODON_DOMAIN = os.environ.get("MASTODON_DOMAIN", "mastodon")
MASTODON_USERNAME = os.environ.get("MASTODON_USERNAME", "bob")
# Mastodon 4.x で OAuth password grant が削除されたため、entrypoint が
# Doorkeeper で発行した access token をファイル経由で受け取る。
MASTODON_TOKEN_FILE = os.environ.get(
    "MASTODON_TOKEN_FILE", "/mastodon-tokens/bob.token"
)

# 連合経路の伝搬は Mastodon の Sidekiq queue 経由なので秒〜10 秒オーダで
# 揺れる。ローカル sqlx 経路は サブ秒。Sidekiq の retry は初回失敗から
# 15-30s 後なので、初回 enqueue が遅れたケースでも吸収できる長さを取る。
# CI runner はホストと比べてさらに遅くなりがちなので 180s を取った。
DEFAULT_POLL_TIMEOUT = int(os.environ.get("FEDERATION_POLL_TIMEOUT", "180"))
DEFAULT_POLL_INTERVAL = float(os.environ.get("FEDERATION_POLL_INTERVAL", "3"))

# self-signed test CA を信頼するため verify=False を全リクエストで使う。
# SSL_CERT_FILE 経由で trust を入れている実装もあるが、httpx は環境変数を
# 拾わないので明示的に切る ── 本番経路は test CA を信頼しないので影響なし。
_SSL_VERIFY = False


def poll_until(
    predicate: Callable[[], Any],
    *,
    timeout: int = DEFAULT_POLL_TIMEOUT,
    interval: float = DEFAULT_POLL_INTERVAL,
    desc: str = "",
):
    """`predicate()` が truthy を返すまで `timeout` 秒間ポーリングする。

    最後の例外を timeout 時メッセージに含めるので、根本原因が wait の中で
    分かりやすい。
    """
    deadline = time.time() + timeout
    last_exc: BaseException | None = None
    last_value: Any = None
    while time.time() < deadline:
        try:
            value = predicate()
            if value:
                return value
            last_value = value
        except Exception as exc:  # noqa: BLE001 - we want everything
            last_exc = exc
        time.sleep(interval)
    msg = f"Poll timed out after {timeout}s"
    if desc:
        msg += f" — {desc}"
    if last_exc is not None:
        msg += f" (last exc: {last_exc!r})"
    elif last_value is not None:
        msg += f" (last value: {last_value!r})"
    raise TimeoutError(msg)


def wait_for_http(
    url: str,
    *,
    status_ok: tuple[int, ...] = (200,),
    timeout: int = 180,
    interval: float = 3.0,
    method: str = "GET",
    headers: dict[str, str] | None = None,
):
    """サービス起動完了まで HTTP GET をリトライする。"""
    deadline = time.time() + timeout
    last_status: int | None = None
    last_exc: BaseException | None = None
    while time.time() < deadline:
        try:
            resp = httpx.request(
                method, url, headers=headers, timeout=5, verify=_SSL_VERIFY
            )
            last_status = resp.status_code
            if resp.status_code in status_ok:
                return
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
        time.sleep(interval)
    detail = f"last_status={last_status} last_exc={last_exc!r}"
    raise TimeoutError(f"{url} not ready within {timeout}s ({detail})")


class SakurasatoClient:
    """Sakurasato ローカル API (UDS + Bearer) と公開 AP エンドポイントを叩く。

    `local` クライアントは UDS 越し、`public` クライアントは https://sakurasato
    (TLS 終端 = nginx-sks) 越し。本物の Fediverse から見た経路は `public` 側。
    """

    def __init__(
        self,
        *,
        base_url: str,
        domain: str,
        socket_path: str,
        token: str,
    ) -> None:
        self.base_url = base_url
        self.domain = domain
        self.socket_path = socket_path
        self.token = token
        # UDS transport で `/api/v1/*` を叩く。Host header は名前付きで何でも
        # よいが、HTTP/1.1 で必須なので `localhost` を載せる。
        self._uds_transport = httpx.HTTPTransport(uds=socket_path)
        self._local = httpx.Client(
            transport=self._uds_transport,
            base_url="http://localhost",
            timeout=15,
            headers={"Authorization": f"Bearer {token}"},
        )
        # 公開側 (= nginx-sks 越し) は test CA + verify=False。
        self._public = httpx.Client(
            base_url=base_url,
            timeout=15,
            verify=_SSL_VERIFY,
        )

    def close(self) -> None:
        self._local.close()
        self._public.close()

    # ── local API ────────────────────────────────────────────
    def whoami(self) -> dict:
        resp = self._local.get("/api/v1/whoami")
        resp.raise_for_status()
        return resp.json()

    def home_timeline(self, *, limit: int = 40) -> list[dict]:
        resp = self._local.get(
            "/api/v1/timeline/home", params={"limit": str(limit)}
        )
        resp.raise_for_status()
        return resp.json().get("notes", resp.json())  # 戻り値 shape は柔軟に

    def create_note(
        self,
        content: str,
        *,
        visibility: str = "public",
        summary: str | None = None,
        in_reply_to_ap_id: str | None = None,
    ) -> dict:
        body: dict[str, Any] = {"content": content, "visibility": visibility}
        if summary is not None:
            body["summary"] = summary
        if in_reply_to_ap_id is not None:
            body["in_reply_to_ap_id"] = in_reply_to_ap_id
        resp = self._local.post("/api/v1/notes", json=body)
        resp.raise_for_status()
        return resp.json()

    def create_reaction(
        self,
        *,
        note_id: int,
        content: str,
    ) -> dict:
        # local API は `note_id` (= local note の BIGSERIAL) を受ける。
        # remote 投稿へのリアクションは未対応 (= 404 が返る)。
        resp = self._local.post(
            "/api/v1/reactions",
            json={"note_id": note_id, "content": content},
        )
        resp.raise_for_status()
        return resp.json()

    def delete_reaction(self, reaction_id: int) -> None:
        resp = self._local.delete(f"/api/v1/reactions/{reaction_id}")
        resp.raise_for_status()

    # ── public AP / WebFinger ────────────────────────────────
    def webfinger(self, acct: str) -> dict:
        resp = self._public.get(
            "/.well-known/webfinger", params={"resource": f"acct:{acct}"}
        )
        resp.raise_for_status()
        return resp.json()

    def actor_json(self, username: str) -> dict:
        resp = self._public.get(
            f"/users/{username}",
            headers={"Accept": "application/activity+json"},
        )
        resp.raise_for_status()
        return resp.json()

    def nodeinfo(self) -> dict:
        wk = self._public.get("/.well-known/nodeinfo")
        wk.raise_for_status()
        links = wk.json().get("links", [])
        if not links:
            raise AssertionError("nodeinfo well-known had no links")
        href = links[0]["href"]
        resp = httpx.get(href, timeout=10, verify=_SSL_VERIFY)
        resp.raise_for_status()
        return resp.json()


class MastodonClient:
    """Mastodon REST API + AP エンドポイントを叩く薄いラッパ。"""

    def __init__(
        self,
        *,
        base_url: str,
        domain: str,
        username: str,
        token: str,
    ) -> None:
        self.base_url = base_url
        self.domain = domain
        self.username = username
        self.http = httpx.Client(base_url=base_url, timeout=20, verify=_SSL_VERIFY)
        self._token = token
        self._account: dict | None = None

    def close(self) -> None:
        self.http.close()

    def login(self) -> dict:
        # トークンは entrypoint で先に作っておく方式に切り替わったので、
        # ここでは verify_credentials を 1 度叩いて疎通確認 + account を貯める
        # だけ。互換性のため戻り値は `verify_credentials` の result。
        self._account = self.verify_credentials()
        return self._account

    @property
    def token(self) -> str:
        if self._token is None:
            raise RuntimeError("MastodonClient is not logged in yet")
        return self._token

    @property
    def account(self) -> dict:
        if self._account is None:
            raise RuntimeError("MastodonClient is not logged in yet")
        return self._account

    def _auth_headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}"}

    # ── REST ─────────────────────────────────────────────────
    def verify_credentials(self) -> dict:
        resp = self.http.get(
            "/api/v1/accounts/verify_credentials", headers=self._auth_headers()
        )
        resp.raise_for_status()
        return resp.json()

    def create_status(
        self,
        content: str,
        *,
        visibility: str = "public",
        spoiler_text: str | None = None,
        in_reply_to_id: str | None = None,
    ) -> dict:
        body: dict[str, Any] = {"status": content, "visibility": visibility}
        if spoiler_text:
            body["spoiler_text"] = spoiler_text
        if in_reply_to_id:
            body["in_reply_to_id"] = in_reply_to_id
        resp = self.http.post(
            "/api/v1/statuses", json=body, headers=self._auth_headers()
        )
        resp.raise_for_status()
        return resp.json()

    def search_accounts(self, q: str, *, resolve: bool = True) -> list[dict]:
        resp = self.http.get(
            "/api/v1/accounts/search",
            params={"q": q, "resolve": "true" if resolve else "false"},
            headers=self._auth_headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def follow(self, account_id: str) -> dict:
        resp = self.http.post(
            f"/api/v1/accounts/{account_id}/follow", headers=self._auth_headers()
        )
        resp.raise_for_status()
        return resp.json()

    def public_timeline(self, *, local: bool = False, limit: int = 40) -> list[dict]:
        params = {"limit": str(limit)}
        if local:
            params["local"] = "true"
        resp = self.http.get(
            "/api/v1/timelines/public",
            params=params,
            headers=self._auth_headers(),
        )
        resp.raise_for_status()
        return resp.json()

    def home_timeline(self, *, limit: int = 40) -> list[dict]:
        resp = self.http.get(
            "/api/v1/timelines/home",
            params={"limit": str(limit)},
            headers=self._auth_headers(),
        )
        resp.raise_for_status()
        return resp.json()

    # ── AP / WebFinger ───────────────────────────────────────
    def webfinger(self, acct: str) -> dict:
        resp = self.http.get(
            "/.well-known/webfinger", params={"resource": f"acct:{acct}"}
        )
        resp.raise_for_status()
        return resp.json()


def _read_token_file(path_str: str, *, label: str) -> str:
    path = Path(path_str)
    if not path.exists():
        raise FileNotFoundError(f"{label} token file not found at {path}")
    raw = path.read_text(encoding="utf-8").strip()
    if not raw:
        raise RuntimeError(f"{path} ({label}) is empty")
    return raw


# ── session fixtures ─────────────────────────────────────────


@pytest.fixture(scope="session", autouse=True)
def wait_for_instances() -> None:
    # sakurasato 側: actor JSON が 200 で返れば最低限 ready (init 完了)。
    wait_for_http(
        f"{SAKURASATO_BASE_URL}/users/me",
        headers={"Accept": "application/activity+json"},
        timeout=120,
    )
    # Mastodon 側: `/api/v1/instance` が 200 で返れば puma が live。
    wait_for_http(f"{MASTODON_BASE_URL}/api/v1/instance", timeout=240)


@pytest.fixture(scope="session")
def sakurasato_token() -> str:
    return _read_token_file(SAKURASATO_TOKEN_FILE, label="sakurasato")


@pytest.fixture(scope="session")
def mastodon_token() -> str:
    return _read_token_file(MASTODON_TOKEN_FILE, label="mastodon")


@pytest.fixture(scope="session")
def sakurasato(sakurasato_token: str):
    client = SakurasatoClient(
        base_url=SAKURASATO_BASE_URL,
        domain=SAKURASATO_DOMAIN,
        socket_path=SAKURASATO_LOCAL_API_SOCKET,
        token=sakurasato_token,
    )
    try:
        yield client
    finally:
        client.close()


@pytest.fixture(scope="session")
def mastodon(mastodon_token: str):
    client = MastodonClient(
        base_url=MASTODON_BASE_URL,
        domain=MASTODON_DOMAIN,
        username=MASTODON_USERNAME,
        token=mastodon_token,
    )
    client.login()
    try:
        yield client
    finally:
        client.close()
