"""Fixtures and helpers for sakurasato federation tests.

Sakurasato 側は **UDS (Unix domain socket) 経由** の Bearer トークン認証付き
ローカル API を叩く ── 公開 TCP には載っていない `/api/v1/*` を本来の経路で
触るため。Mastodon 側は通常の HTTPS OAuth で叩く。

外部要件:

- compose 側で `sakurasato_local_api` 名前付き volume を /home/nonroot に
  マウントしておくこと (= UDS が pytest コンテナから見える)
- `sakurasato-token-issuer` が `SAKURASATO_TOKEN_FILE` (デフォルトは
  `/home/nonroot/pytest.token`、compose の env で上書き) に raw token を
  書いた状態
- `mastodon-entrypoint` が `MASTODON_TOKEN_FILE` (デフォルトは
  `/mastodon-tokens/bob.token`) に Doorkeeper access token を書いた状態
  (Mastodon 4.x で OAuth password grant が削除されたため、起動時に発行する)
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
# [round-2 M-2] docstring と一致させる ── compose env で常に上書きされる
# 想定だが、direct 実行・debug 等で env 無しで走った時に
# `FileNotFoundError: /tokens/pytest.token` で詰まないよう、docstring 記載の
# 実体パスをデフォルトにする。
SAKURASATO_TOKEN_FILE = os.environ.get(
    "SAKURASATO_TOKEN_FILE", "/home/nonroot/pytest.token"
)

MASTODON_BASE_URL = os.environ.get("MASTODON_BASE_URL", "https://mastodon")
MASTODON_DOMAIN = os.environ.get("MASTODON_DOMAIN", "mastodon")
MASTODON_USERNAME = os.environ.get("MASTODON_USERNAME", "bob")
# Mastodon 4.x で OAuth password grant が削除されたため、entrypoint が
# Doorkeeper で発行した access token をファイル経由で受け取る。
MASTODON_TOKEN_FILE = os.environ.get(
    "MASTODON_TOKEN_FILE", "/mastodon-tokens/bob.token"
)

# ── Nekonoverse env (#58 PR2a → PR2b) ────────────────────────
# Nekonoverse は Mastodon 互換 API を提供する。PR2a smoke では bob の
# user-level token は取らず public エンドポイントだけで Follow round-trip を
# 検証した。**PR2b で bob 用 OAuth Bearer の DB 直 seed 経路を追加** ──
# `nekonoverse-bob-issuer` (compose) が `oauth_tokens` テーブルに INSERT した
# raw token を共有 named volume `nekonoverse_tokens` 経由でファイル受領する。
# fixture は `NEKONOVERSE_TOKEN_FILE` env のパスを `_read_token_file` で開く。
NEKONOVERSE_BASE_URL = os.environ.get("NEKONOVERSE_BASE_URL", "https://nekonoverse")
NEKONOVERSE_DOMAIN = os.environ.get("NEKONOVERSE_DOMAIN", "nekonoverse")
NEKONOVERSE_USERNAME = os.environ.get("NEKONOVERSE_USERNAME", "bob")
# PR2b: docstring 記載の実体パスをデフォルトに (compose env で常に上書き)。
# 直接 pytest 起動 / debug 時に env 未設定でも `FileNotFoundError` で
# 即落ちる方が、Bearer 無し silent 401 のデバッグより速いので default に倒す。
NEKONOVERSE_TOKEN_FILE = os.environ.get(
    "NEKONOVERSE_TOKEN_FILE", "/nkv-tokens/bob.token"
)

# どの counterpart instance を待つかを env でゲートする。compose 側で
# 該当しないターゲットを `"0"` に倒すことで、Mastodon stack で立ってない
# Nekonoverse / 逆も含めた wait 失敗を避ける。
# - 既存 Mastodon stack は env を設定しないので `"1"` (= 従来挙動) に倒れる。
# - Nekonoverse stack は MASTODON_ENABLED=0 / NEKONOVERSE_ENABLED=1 を渡す。
MASTODON_ENABLED = os.environ.get("MASTODON_ENABLED", "1") != "0"
NEKONOVERSE_ENABLED = os.environ.get("NEKONOVERSE_ENABLED", "0") == "1"

# 連合経路の伝搬は Mastodon の Sidekiq queue 経由なので秒〜10 秒オーダで
# 揺れる。ローカル sqlx 経路は サブ秒。Sidekiq の retry は初回失敗から
# 15-30s 後なので、初回 enqueue が遅れたケースでも吸収できる長さを取る。
# CI runner はホストと比べてさらに遅くなりがちなので 180s を取った。
DEFAULT_POLL_TIMEOUT = int(os.environ.get("FEDERATION_POLL_TIMEOUT", "180"))
DEFAULT_POLL_INTERVAL = float(os.environ.get("FEDERATION_POLL_INTERVAL", "3"))

# [round-2 M-1] httpx は `SSL_CERT_FILE` 環境変数を **自動では** 拾わない。
# `verify=` に `ssl.SSLContext` を渡すと **自己署名 CA を信頼しつつホスト名・
# チェーン検証を維持** できる ── `verify=False` だと Bearer 込みの通信でも
# 証明書エラーを無音でスルーするのでテスト経路でも望ましくない。
# httpx 0.28+ で `verify=<str>` が DeprecationWarning なので、`SSLContext`
# 直渡しが現行の正解。
# compose 側で `SSL_CERT_FILE: /certs/ca.crt` が常時セットされる前提だが、
# 未設定環境 (= compose 外の手動 debug 等) では `verify=False` にフォールバック
# して「とにかく繋ぐ」を優先する。
def _build_ssl_verify() -> "ssl.SSLContext | bool":
    ca_file = os.environ.get("SSL_CERT_FILE")
    if not ca_file:
        return False
    return ssl.create_default_context(cafile=ca_file)


_SSL_VERIFY = _build_ssl_verify()


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
        # 現行 server は `{ notes: [...], next_before_id: ... }` で返す。
        # 将来 list 直返しに変わっても拾えるよう .get でフォールバック。
        # [review L-2] 対応: 1 回だけ deserialize する。
        data = resp.json()
        return data.get("notes", data) if isinstance(data, dict) else data

    def create_note(
        self,
        content: str,
        *,
        visibility: str = "public",
        summary: str | None = None,
        in_reply_to_ap_id: str | None = None,
        attachment_ids: list[int] | None = None,
    ) -> dict:
        body: dict[str, Any] = {"content": content, "visibility": visibility}
        if summary is not None:
            body["summary"] = summary
        if in_reply_to_ap_id is not None:
            body["in_reply_to_ap_id"] = in_reply_to_ap_id
        if attachment_ids is not None:
            body["attachment_ids"] = attachment_ids
        resp = self._local.post("/api/v1/notes", json=body)
        resp.raise_for_status()
        return resp.json()

    def upload_media(
        self,
        *,
        body: bytes,
        kind: str = "attachment",
        content_type: str = "image/png",
        alt: str | None = None,
    ) -> dict:
        """`POST /api/v1/media?kind=...` ── 生バイト列を投げて `MediaResponse` を受ける。

        サーバが media-proxy 経由で再エンコードするので、入力は PNG / JPEG /
        WebP / GIF など `image` crate がデコードできる形式なら何でも良い。
        返り値の `id` を `create_note(attachment_ids=[id])` に渡して投稿に
        紐付ける。
        """
        params: dict[str, str] = {"kind": kind}
        if alt is not None:
            params["alt"] = alt
        resp = self._local.post(
            "/api/v1/media",
            params=params,
            content=body,
            headers={"Content-Type": content_type},
        )
        resp.raise_for_status()
        return resp.json()

    def fetch_media(self, url: str) -> httpx.Response:
        """`/media/<key>` を公開経路 (= 非認証) で叩く。

        Mastodon の media downloader はここを Bearer 無しで叩くので、
        regression test for PR #143 (followers-only attachment の 404 over-restriction)
        は本メソッドで `https://sakurasato/media/<key>.webp` を直接叩いて
        200 を確認する。
        """
        # `_public` は base_url = https://sakurasato。url は full URL でも path でも
        # 良いが、AP `attachment[].url` で配送されるのは絶対 URL なので
        # そのまま渡す。
        return self._public.get(url)

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

    # ── Issue #66 / M12: 鍵アカ ── lock/unlock + follow-request 管理 ───────
    def actor_lock(self) -> dict:
        resp = self._local.post("/api/v1/actor/lock")
        resp.raise_for_status()
        return resp.json()

    def actor_unlock(self) -> dict:
        resp = self._local.post("/api/v1/actor/unlock")
        resp.raise_for_status()
        return resp.json()

    def follow_requests(self, state: str = "pending") -> list[dict]:
        """`state`: `pending` (default) / `all`。`all` は accepted/rejected も含む。"""
        resp = self._local.get(
            "/api/v1/follow-requests", params={"state": state}
        )
        resp.raise_for_status()
        return resp.json().get("items", [])

    def follow_request_approve(self, follow_id: int) -> dict:
        resp = self._local.post(f"/api/v1/follow-requests/{follow_id}/approve")
        resp.raise_for_status()
        return resp.json()

    def follow_request_reject(self, follow_id: int) -> dict:
        resp = self._local.post(f"/api/v1/follow-requests/{follow_id}/reject")
        resp.raise_for_status()
        return resp.json()

    def follow_request_delete(self, follow_id: int) -> None:
        """**PR #80 round-2 #6 (test re-runnability)**: 古い follow 行をハード削除する。"""
        resp = self._local.delete(f"/api/v1/follow-requests/{follow_id}")
        if resp.status_code == 404:
            return  # 既に無い → no-op
        resp.raise_for_status()

    def following(self, *, limit: int = 40) -> list[dict]:
        """`GET /api/v1/following` ── accepted な follow 先一覧 (M13 PR3)。

        #58 PR2a: TUI で `:follow @bob` した結果 Accept まで通ったかの assertion
        に使う ── status line は「follow requested」のまま遷移しないので、
        local API の following list を polling して accepted を確認する。

        サーバの wire shape は `{entries, next_before_id}` で、各 entry は
        `{follow_id, follow_state, follow_created_at, actor}` (= ``ActorRow``)。
        """
        resp = self._local.get(
            "/api/v1/following", params={"limit": str(limit)}
        )
        resp.raise_for_status()
        data = resp.json()
        return data.get("entries", data) if isinstance(data, dict) else data

    def follow(self, acct: str) -> dict:
        """`POST /api/v1/follow` ── M13 PR2 由来。**冪等** (PR #114 で重複防止)。

        body は `{"acct": "user@host"}`。レスポンスに `already_accepted` /
        `already_pending` フラグが乗るので、fixture から「既に follow 済」を
        判定して poll をスキップできる。
        """
        resp = self._local.post("/api/v1/follow", json={"acct": acct})
        resp.raise_for_status()
        return resp.json()

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
        # Mastodon の `resolve=true` は内部で WebFinger + actor fetch を同期
        # 実行する。相手側 (= sakurasato) がまだ Mastodon のキャッシュに
        # 入っていないタイミング (= 起動直後など) で resolve が時々 **422
        # `unprocessable_content`** を返してくる ── Mastodon 内部例外を
        # rescue した結果なので、リトライすれば通る。1 回失敗で test が
        # 落ちないよう、最大数回の HTTP リトライをここで吸収する。
        # (poll_until のような業務ロジック側ループより、HTTP layer の retry の
        #  方が他の使い手に対しても効くため)
        last_status: int | None = None
        last_body: str | None = None
        for _ in range(10):
            resp = self.http.get(
                "/api/v1/accounts/search",
                params={"q": q, "resolve": "true" if resolve else "false"},
                headers=self._auth_headers(),
            )
            if resp.status_code == 200:
                return resp.json()
            last_status, last_body = resp.status_code, resp.text[:200]
            if resp.status_code not in (422, 503, 504):
                break
            time.sleep(2)
        raise RuntimeError(
            f"Mastodon /accounts/search refused query q={q!r} "
            f"resolve={resolve} after retries: status={last_status} body={last_body!r}"
        )

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
    # `MASTODON_ENABLED=0` で skip 可能 (= Nekonoverse stack 等で立てない時)。
    if MASTODON_ENABLED:
        wait_for_http(f"{MASTODON_BASE_URL}/api/v1/instance", timeout=240)
    # Nekonoverse 側: 同じく `/api/v1/instance` が 200 で ready。
    if NEKONOVERSE_ENABLED:
        wait_for_http(f"{NEKONOVERSE_BASE_URL}/api/v1/instance", timeout=240)


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


# ── Nekonoverse: Mastodon 互換 API クライアント (#58 PR2a → PR2b) ───
#
# Nekonoverse は Mastodon-style `/api/v1/*` を喋るので shape は近いが、
# 接続先 / fixture 名 / 認証経路が違うので別クラスにする
# (MastodonClient を継承しない ── 将来 Nekonoverse 固有 endpoint が増えた
# ときに同 class で破綻させないため)。
#
# **PR2b**: bob の user-level OAuth Bearer を `oauth_tokens` 直 seed で受領
# できるようになったので、auth 必須エンドポイント (status post / reaction /
# verify_credentials) を生やす。public-only PR2a 経路 (`lookup_account` /
# `followers`) は破壊しないよう同居させる。`token` は optional ── public-only
# テストは token 渡さずインスタンス化可能。
class NekonoverseClient:
    """Nekonoverse REST API + AP エンドポイントを叩く薄いラッパ。

    `token` を渡すと auth 必須エンドポイント (`create_status` /
    `verify_credentials` / `get_status` 等) が叩けるようになる。token 無しでも
    public エンドポイント (`lookup_account` / `followers` / `webfinger`) は
    引き続き使える ── PR2a の `test_nekonoverse_tui` がそのまま壊れない。
    """

    def __init__(
        self,
        *,
        base_url: str,
        domain: str,
        username: str,
        token: str | None = None,
    ) -> None:
        self.base_url = base_url
        self.domain = domain
        self.username = username
        self._token = token
        self.http = httpx.Client(base_url=base_url, timeout=20, verify=_SSL_VERIFY)

    def close(self) -> None:
        self.http.close()

    # ── auth helper ──────────────────────────────────────────
    @property
    def token(self) -> str:
        if self._token is None:
            raise RuntimeError(
                "NekonoverseClient was constructed without a token "
                "but an auth-required endpoint was called"
            )
        return self._token

    def _auth_headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}"}

    # ── public Mastodon-compat ───────────────────────────────
    def lookup_account(self, acct: str) -> dict:
        """`GET /api/v1/accounts/lookup` ── acct → account JSON (public)。

        Mastodon 仕様で auth 不要。Bob の id を取り出して
        `followers(id)` に渡す経路。`acct` は `local-username` 形式
        (= `bob`) でも `user@domain` 形式でも引ける。
        """
        resp = self.http.get(
            "/api/v1/accounts/lookup", params={"acct": acct}
        )
        resp.raise_for_status()
        return resp.json()

    def followers(self, account_id: str) -> list[dict]:
        """`GET /api/v1/accounts/{id}/followers` ── public。Follow + Accept の検証で使う。"""
        resp = self.http.get(f"/api/v1/accounts/{account_id}/followers")
        resp.raise_for_status()
        return resp.json()

    # ── auth required (PR2b) ─────────────────────────────────
    def verify_credentials(self) -> dict:
        """`GET /api/v1/accounts/verify_credentials` ── token が valid か疎通確認。"""
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
        """`POST /api/v1/statuses` ── bob として note 投稿。

        sks がフォロー済みなら bob の public note は sks 側に federate される。
        `id` (UUID) / `uri` (AP id) を取り出して、sks の home timeline で同じ
        note が見えるかの assertion 鍵にする。
        """
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

    def get_status(self, status_id: str) -> dict:
        """`GET /api/v1/statuses/{id}` ── bob の view から見た note JSON。

        reaction が sks → nkv に届いたかを `reactions` (Misskey 互換) /
        `favourites_count` (Mastodon 互換) どちらかで読む経路。
        """
        resp = self.http.get(
            f"/api/v1/statuses/{status_id}", headers=self._auth_headers()
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


@pytest.fixture(scope="session")
def nekonoverse_token() -> str:
    return _read_token_file(NEKONOVERSE_TOKEN_FILE, label="nekonoverse")


@pytest.fixture(scope="session")
def nekonoverse(nekonoverse_token: str):
    client = NekonoverseClient(
        base_url=NEKONOVERSE_BASE_URL,
        domain=NEKONOVERSE_DOMAIN,
        username=NEKONOVERSE_USERNAME,
        token=nekonoverse_token,
    )
    try:
        yield client
    finally:
        client.close()


# ── tmux driver fixtures (#58 PR2a) ───────────────────────────
#
# `scripts/tmux-e2e/conftest.py` を本 image では `/tests/tmux_driver.py`
# として配置している (Dockerfile.tmux 参照)。`TmuxSession` / `_run_lib` /
# `_unique_session_name` を import して、ローカル fixture として再エクスポート
# する。`SAKURASATO_TUI_BIN` env でバイナリ path を上書き可能。
#
# fixture を本 conftest で生やすのは collection 順を制御するため
# (= import side effect で session 起動 fixture を勝手に増やしたくない)。
try:
    from tmux_driver import TmuxSession as _TmuxSession  # noqa: F401
    from tmux_driver import _run_lib as _tmux_run_lib  # type: ignore[attr-defined]
    from tmux_driver import _unique_session_name as _tmux_session_name  # type: ignore[attr-defined]
    _TMUX_DRIVER_AVAILABLE = True
except ImportError:
    _TMUX_DRIVER_AVAILABLE = False


@pytest.fixture
def tmux_tui():
    """`/usr/local/bin/sakurasato-tui` を tmux pty 内で起動する factory。

    Usage::

        def test_smoke(tmux_tui, sakurasato_socket_path, sakurasato_token_file):
            tui = tmux_tui(sakurasato_socket_path, sakurasato_token_file)
            tui.wait_until_text("timeline", 30)
            tui.send_keys(":quit", "Enter")

    引数:

    - ``socket_path``: server の UDS パス。`SAKURASATO_SOCKET` env で TUI に渡る。
    - ``token_file``: Bearer トークンファイル path。`--token-file` 引数で渡る。
    - ``*extra_args``: 例えば ``"--no-images"`` (CI のテキスト UI 強制)。
    - ``label``: tmux session 名のヒント。

    tmux / TUI binary が無い環境では fixture 取得時に `pytest.skip()` する
    (= mastodon-only stack で誤って collect された時の保護)。
    """
    if not _TMUX_DRIVER_AVAILABLE:
        pytest.skip("tmux_driver helper not available — wrong test image?")

    started: list = []

    def factory(
        socket_path: str,
        token_file: str,
        *extra_args: str,
        label: str = "tui",
    ):
        bin_path = os.environ.get("SAKURASATO_TUI_BIN", "sakurasato-tui")
        name = _tmux_session_name(label)
        cmd = (
            "env",
            f"SAKURASATO_SOCKET={socket_path}",
            bin_path,
            "--token-file",
            token_file,
            *extra_args,
        )
        _tmux_run_lib("tmux_start", name, *cmd)
        session = _TmuxSession(name)
        started.append(session)
        return session

    try:
        yield factory
    finally:
        for s in started:
            s.kill()


@pytest.fixture(scope="session")
def sakurasato_socket_path() -> str:
    """`SakurasatoClient.socket_path` と同じ値を fixture 化して TUI 側にも渡せるように。"""
    return SAKURASATO_LOCAL_API_SOCKET


@pytest.fixture(scope="session")
def sakurasato_token_file() -> str:
    """`SAKURASATO_TOKEN_FILE` のパス本体 (= `--token-file` に渡す用)。"""
    return SAKURASATO_TOKEN_FILE
