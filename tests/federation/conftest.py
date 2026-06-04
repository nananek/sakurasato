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

import json
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

# ── Misskey env (#162 / M14 MiAuth parity test 基盤) ─────────────
# 本物 Misskey instance を `compose/docker-compose.federation-misskey.yml`
# pytest profile で起動し、`misskey-seed` が admin user + `i` token を発行する。
# fixture は `MISSKEY_TOKEN_FILE` から token を読み、`misskey.py` (= YuzuRyo61
# 製 MIT) でクライアントを構築する。
#
# AGPL discipline: `misskey/misskey:latest` の **未改変 run** は contagion 無し。
# Python client lib は **YuzuRyo61/Misskey.py (MIT)** のみ採用し
# `AmaseCocoa/misskey-py` (AGPL) は絶対に依存させない (= [[agpl-discipline-miauth]])。
MISSKEY_BASE_URL = os.environ.get("MISSKEY_BASE_URL", "https://misskey")
MISSKEY_DOMAIN = os.environ.get("MISSKEY_DOMAIN", "misskey")
MISSKEY_USERNAME = os.environ.get("MISSKEY_USERNAME", "admin")
MISSKEY_TOKEN_FILE = os.environ.get(
    "MISSKEY_TOKEN_FILE", "/misskey-tokens/admin.token"
)
MISSKEY_ENABLED = os.environ.get("MISSKEY_ENABLED", "0") == "1"

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
    # Misskey 側 (= #162): `/api/meta` を空 body POST で叩いて 200 が返れば ready。
    # Misskey API は GET ではなく POST + JSON body が前提 (= 唯一の例外は
    # `/api/ping` だがそれも POST)。`wait_for_http` は GET 専用なので Misskey は
    # 直接 httpx で probe する。
    if MISSKEY_ENABLED:
        deadline = time.time() + 240
        last_exc: BaseException | None = None
        last_status: int | None = None
        while time.time() < deadline:
            try:
                resp = httpx.post(
                    f"{MISSKEY_BASE_URL}/api/meta",
                    json={},
                    timeout=5,
                    verify=_SSL_VERIFY,
                )
                last_status = resp.status_code
                if resp.status_code == 200:
                    break
            except Exception as exc:  # noqa: BLE001
                last_exc = exc
            time.sleep(3)
        else:
            raise TimeoutError(
                f"Misskey not ready within 240s "
                f"(last_status={last_status} last_exc={last_exc!r})"
            )


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

    def following(self, account_id: str, *, limit: int = 80) -> list[dict]:
        """``GET /api/v1/accounts/{id}/following`` ── public。

        #140 PR2 (Scenario A) で bob が Move 後に alice@sakurasato-new を
        follow 状態になることを観測する経路。Mastodon spec で list[Account]
        を返し、各エントリに ``acct`` / ``username`` / ``url`` 等が乗る。
        `limit` は Mastodon の default 40 を超えるケースに備えて 80 に倒す。
        """
        resp = self.http.get(
            f"/api/v1/accounts/{account_id}/following",
            params={"limit": limit},
        )
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

    def status_context(self, status_id: str) -> dict:
        """`GET /api/v1/statuses/{id}/context` ── note の親子 thread を取る。

        Mastodon 仕様で ``{ "ancestors": [...], "descendants": [...] }`` を
        返す。Issue #138 シナリオ A (= sks → nkv reply) で、bob 側 status
        の descendants に alice の reply が出現するまで polling する経路で
        使う。auth は ``get_status`` と揃え、Bearer 必須にしておく ──
        bob token がある前提なので追加コストはなく、reaction 系 helper と
        同じ防御線で動く。
        """
        resp = self.http.get(
            f"/api/v1/statuses/{status_id}/context", headers=self._auth_headers()
        )
        resp.raise_for_status()
        return resp.json()

    # ── Move (#140 PR1 / Scenario B) ─────────────────────────
    def register_app(
        self,
        *,
        client_name: str = "sakurasato-e2e",
        scopes: str = "read write follow",
    ) -> dict:
        """``POST /api/v1/apps`` → ``POST /oauth/token`` (client_credentials)
        の 2 段を 1 関数で済ませる。返り値は ``{client_id, client_secret,
        access_token}`` 形式の dict ── `register_account` への `app_token`
        として ``access_token`` を渡す経路。
        """
        app_resp = self.http.post(
            "/api/v1/apps",
            json={
                "client_name": client_name,
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": scopes,
            },
        )
        app_resp.raise_for_status()
        app = app_resp.json()
        token_resp = self.http.post(
            "/oauth/token",
            json={
                "grant_type": "client_credentials",
                "client_id": app["client_id"],
                "client_secret": app["client_secret"],
                "scope": scopes,
            },
        )
        token_resp.raise_for_status()
        return {
            "client_id": app["client_id"],
            "client_secret": app["client_secret"],
            "access_token": token_resp.json()["access_token"],
        }

    def register_account(
        self,
        *,
        app_token: str,
        username: str,
        email: str,
        password: str,
    ) -> dict:
        """``POST /api/v1/accounts`` ── 新規アカウントを Mastodon 互換で登録。

        2nd 用 OAuth app を別途立てる手間を省くため、呼び出し側で
        `register_app` で取った `app_token` を渡してもらう。返り値は
        Mastodon 仕様の Token (`{access_token, token_type, scope, ...}`)
        ── このアカウントとして login 済の Bearer として直後の auth 操作
        (= `update_credentials_also_known_as`) に使える。
        """
        payload = {
            "username": username,
            "email": email,
            "password": password,
            "agreement": True,
            "locale": "en",
        }
        resp = self.http.post(
            "/api/v1/accounts",
            json=payload,
            headers={"Authorization": f"Bearer {app_token}"},
        )
        resp.raise_for_status()
        return resp.json()

    def update_credentials_also_known_as(
        self, *, token: str, also_known_as: list[str]
    ) -> dict:
        """``PATCH /api/v1/accounts/update_credentials`` で `also_known_as`
        を更新する。Nekonoverse は Form 入力で受けるので
        ``multipart/form-data`` で JSON 配列を文字列として送る。

        Move 受領 (sks 側 `handle_move`) が target.alsoKnownAs に signer を
        含むことを要求するため、Scenario B (#140 PR1) で bob_new に bob の
        AP id を 1 件積むのに使う。
        """
        resp = self.http.patch(
            "/api/v1/accounts/update_credentials",
            files={"also_known_as": (None, json.dumps(also_known_as))},
            headers={"Authorization": f"Bearer {token}"},
        )
        resp.raise_for_status()
        return resp.json()

    def initiate_move(self, *, token: str, target_ap_id: str) -> dict:
        """``POST /api/v1/accounts/move`` で **このアカウント本人** から
        ``target_ap_id`` へのアカウント引っ越しを開始する (= Nekonoverse の
        ``initiate_move`` 経路、`activitypub/renderer.render_move_activity`)。

        nkv は target の AP JSON を fetch し、`alsoKnownAs` に自分が含まれて
        いるか検証 → 自身に `moved_to_ap_id` を立て → followers 全 inbox に
        `Move` activity を配送する。`Bearer` を明示渡しできるよう
        `_auth_headers` ではなく引数の `token` を使う ── 2nd actor (= bob_new)
        の token と切り替えたい場面が多い。

        成功は `raise_for_status()` で 2xx を境にする。返却 body の中身
        (= 現状 ``{"ok": true}``、Mastodon 仕様の空オブジェクト、将来の
        ``204 No Content`` まで含めて) には依存しない ── 呼び出し側も
        Move 伝播は sks 側の DB 観測で判定するため、ここで `resp.json()` を
        呼ばず `{}` 固定で返す (PR #194 round-2 ⚠️ #1 対応)。
        """
        resp = self.http.post(
            "/api/v1/accounts/move",
            json={"target_ap_id": target_ap_id},
            headers={"Authorization": f"Bearer {token}"},
        )
        resp.raise_for_status()
        return {}

    def lookup_status(self, url: str) -> dict | None:
        """remote note の URL を nkv 側の local status 行に解決する。

        Mastodon `GET /api/v2/search?q=<url>&resolve=true&type=statuses` を
        叩き、`statuses[]` の先頭を返す。空なら ``None``。Issue #138 シナリオ
        B (= nkv → sks reply) で「sks alice の note の ap_id を渡して bob
        側で `in_reply_to_id` に使える local status id を引く」経路で使う。

        ``resolve=true`` を付けるのは、nkv がまだ取り込んでいない remote
        note でも fetch して取り込むため (= 連合配送に依存しない決定論的経路)。
        ``Authorization`` Bearer を付けると `resolve` が許可される
        (= Mastodon 仕様で resolve は要 auth)。
        """
        resp = self.http.get(
            "/api/v2/search",
            params={"q": url, "resolve": "true", "type": "statuses"},
            headers=self._auth_headers(),
        )
        resp.raise_for_status()
        data = resp.json()
        statuses = data.get("statuses") or []
        return statuses[0] if statuses else None

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


# ── Misskey: 本物の Misskey instance に対する parity test 基盤 (#162) ───
#
# `misskey.py` (= **YuzuRyo61/Misskey.py**, MIT, 89★) で書く wire-compat 比較
# クライアント。**AmaseCocoa/misskey-py は AGPL** なので絶対に依存させない
# ([[agpl-discipline-miauth]] / `requirements.txt` で `misskey.py` を pin)。
#
# fixture スコープ:
#
# - `misskey_token`: admin token をファイルから 1 度読む (session scope)
# - `misskey_instance`: dict-like で `base_url` / `domain` / `username` /
#   `token` を保持。直 httpx で叩く parity test 用
# - `misskey_py_client`: `misskey.Misskey` インスタンス。token 認証済み
#
# parity test (= `test_miauth_flow_parity.py`) は **両 instance 同形** で
# 動作を駆動し、レスポンス schema を diff する。
class MisskeyInstance:
    """Dataclass-like ベース ── parity test は dict よりも attribute access の
    方が読みやすいので軽量クラスで持つ。
    """

    def __init__(self, *, base_url: str, domain: str, username: str, token: str) -> None:
        self.base_url = base_url
        self.domain = domain
        self.username = username
        self.token = token
        # 直叩き用 httpx Client (= MiAuth landing は misskey.py が知らない経路
        # なので直接叩く)。`verify` は test CA を信頼させた SSLContext。
        self.http = httpx.Client(base_url=base_url, timeout=20, verify=_SSL_VERIFY)

    def close(self) -> None:
        self.http.close()

    # ── MiAuth landing (= 公開 web page、auth 不要) ────────────────
    def miauth_landing(
        self,
        uuid: str,
        *,
        name: str | None = None,
        permission: str | None = None,
        callback: str | None = None,
    ) -> httpx.Response:
        params: dict[str, str] = {}
        if name is not None:
            params["name"] = name
        if permission is not None:
            params["permission"] = permission
        if callback is not None:
            params["callback"] = callback
        return self.http.get(f"/miauth/{uuid}", params=params)

    def miauth_check(self, uuid: str) -> httpx.Response:
        """`POST /api/miauth/{uuid}/check`. body 無し、204 / 200 / 404 を返す。"""
        return self.http.post(f"/api/miauth/{uuid}/check")

    def api_i(self, *, token: str | None = None) -> httpx.Response:
        """`POST /api/i { i: <token> }`. token 指定無しなら self.token を使う。"""
        body = {"i": token if token is not None else self.token}
        return self.http.post(
            "/api/i", json=body, headers={"Content-Type": "application/json"}
        )


@pytest.fixture(scope="session")
def misskey_token() -> str:
    return _read_token_file(MISSKEY_TOKEN_FILE, label="misskey")


@pytest.fixture(scope="session")
def misskey_instance(misskey_token: str):
    """parity test 用 ── 直叩き用 httpx Client + admin token を持つ。"""
    inst = MisskeyInstance(
        base_url=MISSKEY_BASE_URL,
        domain=MISSKEY_DOMAIN,
        username=MISSKEY_USERNAME,
        token=misskey_token,
    )
    try:
        yield inst
    finally:
        inst.close()


@pytest.fixture(scope="session")
def misskey_py_client(misskey_token: str):
    """`misskey.Misskey` (= YuzuRyo61/Misskey.py) インスタンス。
    `i` token 認証済みで `mk.i()` / `mk.notes_create()` 等が叩ける。

    **未インポート時は test を skip** ── compose 外の手動 debug で
    `misskey-py` が入っていない環境でも他 fixture が引けるように。
    """
    try:
        from misskey import Misskey  # type: ignore[import-untyped]
    except ImportError:
        pytest.skip("misskey-py (YuzuRyo61/Misskey.py) is not installed")

    # `Misskey` ctor は (`address`, `i=...`) を取る。`address` は scheme 無しの
    # host 名なので URL から組み直す。`session` 引数で httpx 互換 session を
    # 注入できるが test CA 信頼は env (= `SSL_CERT_FILE` 経由で requests/urllib3)
    # で吸収させる ── REQUESTS_CA_BUNDLE が compose env で常に設定される前提。
    addr = MISSKEY_BASE_URL.removeprefix("https://").removeprefix("http://").rstrip("/")
    try:
        return Misskey(address=addr, i=misskey_token)
    except Exception as exc:  # noqa: BLE001
        pytest.skip(f"misskey-py client construction failed: {exc!r}")
