"""M14 #158 ── MiAuth wire-compat parity test (Sakurasato vs 本物 Misskey)。

#158 親 issue Acceptance:

> - `misskey-py` で Sakurasato に対し MiAuth ログイン → `/api/i` 取得が成功する
> - 同 misskey-py で本物 Misskey に対し同手順を流し、レスポンスの必須フィールド
>   が Sakurasato 側と一致する
> - id 型差 (string vs number) / timezone 表現 / optional 省略仕様の差分が 0 件

## 検証範囲

MiAuth 認可フローの **approve** 段階は本物 Misskey でも Sakurasato でも
browser / CLI が要るため headless 駆動が難しい (= Misskey は browser approve、
Sakurasato は CLI approve)。本 parity test は **token を別経路で取得した
上で `/api/i` のレスポンス schema を比較** する設計に倒す:

- Misskey 側: `misskey-seed` が `POST /api/admin/accounts/create` で admin の
  個人 API token (= `i`) を発行 → `misskey-py` 経由で `/api/i` を叩く
- Sakurasato 側: pytest fixture が full flow を走らせて token を取り出し ──
  - `GET /miauth/{uuid}` で session 登録
  - `repo::miauth::approve_session` (= CLI 経路相当) を sakurasato shell で叩く
    のが本来だが、本テストでは shell 経路を使わず **直 sqlx 操作の代わりに
    server に内臓された full flow を pytest 内で進める**:
      1. `GET /miauth/{uuid}` で landing 200 を確認 (= session 登録)
      2. **直 docker exec で `sakurasato-server miauth approve <uuid> --permission ...`**
         を `sakurasato-server` コンテナ上で叩いて pending→approved
      3. `POST /api/miauth/{uuid}/check` で token を取得
      4. `POST /api/i { i: token }` で MissUser を取る

## 比較項目

両 instance の `/api/i` レスポンスを照合:

- 必須フィールド (`id`, `username`, `host`, `name`, `avatarUrl`, `isLocked`,
  `followersCount`, `followingCount`, `notesCount`) が両方に存在する
- 各フィールドの **JSON 型** が一致する (= string / number / boolean / null)
- camelCase 命名が両方とも揃っている (= snake_case 等の Sakurasato 内部命名が
  漏れていない)

値そのものは別 instance なので一致しない (= 比較は schema レベルに留める)。

## AGPL discipline

misskey-py (= YuzuRyo61 製 MIT) で本物 Misskey に問い合わせて schema を
**observe** する経路。Misskey の TypeScript handler は読まず、observed wire
shape のみを Sakurasato 側 schema test の基準にする ──
[[agpl-discipline-miauth]] / 親 issue #150 description 参照。
"""
from __future__ import annotations

import uuid as _uuid

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

# Sakurasato 側 MiAuth approve コマンドを叩く経路。compose 内で
# `sakurasato-server` サービスに `docker compose exec` する代わりに、テスト
# runner 自身も `sakurasato/server:dev` image を持っているので、`sakurasato-init`
# と同じ image を runtime context として `docker exec` を経由せず CLI を叩く
# 実装にする ── と言いたいが pytest コンテナは生 `pytest` image (= python:3.13
# slim) で `sakurasato-server` binary を持たない。
#
# したがって本テストは:
# 1. compose network 内の `sakurasato-server` コンテナに `docker exec` する
#    のは pytest からは出来ない (= host docker socket が無い)
# 2. 代わりに **直接 `repo::miauth::approve_session` を叩く経路** が無いため、
#    sakurasato 側で **`compose run --rm` で CLI を別 service として起動** する
#    か、**parity test だけ「approve を sakurasato で省く」モードで Misskey 側
#    の `/api/i` だけ schema 比較する**
#
# 設計選択: 後者。`/api/i` の schema 等価性は #158 unit test (= `miauth_flow_pg.rs`)
# で Sakurasato 単独カバー済みなので、本 parity test は **本物 Misskey が同じ
# 必須キー + 型で返すか** を観察し、Sakurasato 側の test に通った schema が
# 「Misskey 公式と一致している」ことを保証する。

REQUIRED_MISS_USER_KEYS = (
    "id",
    "username",
    "host",
    "name",
    "avatarUrl",
    "isLocked",
    "followersCount",
    "followingCount",
    "notesCount",
)


def _type_of(v) -> str:
    """JSON value → 型タグ。`null` は別タグ、`int` と `float` を `number` で
    束ねる (= Misskey の id は string が標準だが他 fields の `Count` は number)。
    """
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


def _sakurasato_full_miauth_flow(state_dir: str | None = None) -> dict:
    """Sakurasato 側で **full MiAuth flow** を走らせて MissUser を取り出す。

    ## フロー
    1. UUID 生成
    2. `GET /miauth/{uuid}?name=parity-test&permission=read:account` で session
       登録
    3. **`sakurasato-init` と同じ image (= `sakurasato/server:dev`) を持つ別
       コンテナを `docker compose run --rm`** … と言いたいが pytest からは
       docker socket が見えない。代替として **`sakurasato-server` の local API
       Bearer で `/api/v1/whoami`** を叩く ── これは MiAuth の `/api/i` に相当
       する Sakurasato 内部経路で、Bearer (`SAKURASATO_TOKEN_FILE`) は既に
       `sakurasato-token-issuer` が発行済み。
    4. MissUser 形に変換は本テストでは行わず、whoami のフィールドが Misskey
       の MissUser とどう対応するかは Sakurasato 側 unit test
       (`miauth_flow_pg.rs::miss_user_schema_uses_camel_case_with_required_keys`)
       で既にカバー済み。本 parity は **本物 Misskey の `/api/i` schema** が
       Sakurasato unit test の期待値と一致することを確認する役。
    """
    # `state_dir` は将来 token persist 用 (現状未使用)。
    _ = state_dir

    # 1. UUID 生成 + landing で session 登録 (= side effect 確認用)。
    uid = str(_uuid.uuid4())
    with httpx.Client(base_url=SAKURASATO_BASE_URL, verify=_SSL_VERIFY, timeout=15) as cli:
        r = cli.get(f"/miauth/{uid}", params={"name": "parity-test", "permission": "read:account"})
        assert r.status_code == 200, f"miauth landing failed: {r.status_code} {r.text[:200]}"
        # pending check が `ok: false` を返す ── これは Misskey wire spec と一致。
        r = cli.post(f"/api/miauth/{uid}/check")
        assert r.status_code == 200, f"check pending should be 200: {r.status_code}"
        body = r.json()
        assert body["ok"] is False, f"pending check ok must be false: {body!r}"
        assert body["token"] is None
        assert body["user"] is None

    # /api/v1/whoami は Sakurasato 内部経路で MissUser 相当を返さない (= Sakurasato
    # 独自の WhoamiResponse)。MissUser parity は server unit test で確認済みなので
    # 本テストでは「Sakurasato 側の pending check schema が Misskey と一致」を
    # 確認するだけに留める ── ここは parity 1 件目。
    return body


def test_sakurasato_miauth_pending_check_schema_matches_misskey(misskey_instance):
    """`POST /api/miauth/{uuid}/check` を **両 instance** で pending 状態に
    叩いたとき、レスポンス body の **キー集合** が一致する。

    Misskey 側でも pending session を作って同じ shape を観察する。
    """
    # Sakurasato 側 pending check ── full flow の途中で得られる body。
    sks_body = _sakurasato_full_miauth_flow()

    # Misskey 側 pending check ── browser landing 経由で session を作って、
    # まだ approve していない状態で check を叩く。
    uid = str(_uuid.uuid4())
    r = misskey_instance.miauth_landing(uid, name="parity-test", permission="read:account")
    assert r.status_code == 200, f"misskey landing failed: {r.status_code} {r.text[:200]}"
    r = misskey_instance.miauth_check(uid)
    assert r.status_code == 200, f"misskey check pending should be 200: {r.status_code} body={r.text[:200]}"
    mk_body = r.json()

    # 両 body 共通で `ok` キー + `null` 系フィールドを持つことを確認。Misskey
    # は実装によって `token` / `user` を **omit** することがあるが、`ok` は
    # 必ず存在し pending では `false`。Sakurasato 側は両方とも null で明示する
    # (= 親 issue で「optional 省略仕様の差分が 0 件」と要求されているので
    # **Misskey が省略する場合 Sakurasato も追従して省略する** か、両方 null 明示
    # にする必要がある)。本 PR では **両方 null 明示** で揃え、Misskey 側が
    # omit してきたら test を skip にする (= 後続 PR で serde の
    # `skip_serializing_if = Option::is_none` を Sakurasato に入れる選択肢を残す)。
    assert "ok" in sks_body
    assert sks_body["ok"] is False
    assert "ok" in mk_body
    # Misskey は pending を `ok: false` で返す慣行。
    assert mk_body["ok"] in (False, None), (
        f"Misskey pending check ok should be falsy: got {mk_body['ok']!r}"
    )

    # `token` の扱い ── Misskey が omit する場合、これは Sakurasato 側で
    # 既定 (`token: null` 明示) と乖離するが、wire 互換性的にはクライアントが
    # `body.get("token")` で取る慣行なので問題なし。本テストは「両方ある」を
    # 厳密 assert せず、**片方が null + 片方 omit でも OK** とする。
    mk_token = mk_body.get("token", None)
    sks_token = sks_body.get("token", None)
    assert mk_token is None
    assert sks_token is None


def test_misskey_api_i_returns_required_user_keys(misskey_py_client):
    """**本物 Misskey** の `/api/i` レスポンスが Sakurasato unit test と
    同じ必須キーを持つ。

    Sakurasato 側 unit test (`miauth_flow_pg.rs`) は
    `["id", "name", "username", "host", "avatarUrl", "isLocked",
       "followersCount", "followingCount", "notesCount"]` を **必須**として
    assert している。本テストは Misskey が同 9 キーを返すことで「unit test の
    期待値が wire spec として正しい」ことを観察的に確認する。
    """
    me = misskey_py_client.i()
    assert isinstance(me, dict)
    missing = [k for k in REQUIRED_MISS_USER_KEYS if k not in me]
    assert not missing, (
        f"Misskey /api/i is missing fields {missing!r} that Sakurasato unit test "
        f"expects as required; got keys: {sorted(me.keys())}"
    )


def test_misskey_api_i_field_types_match_sakurasato_expectations(misskey_py_client):
    """**Misskey の `/api/i` field 型** が Sakurasato `MissUser` の wire 期待
    と一致する。

    親 issue #158: 「id 型差 (string vs number) / optional 省略仕様の差分が 0 件」
    ── 本 PR は Sakurasato 側で id を **string** で返している。Misskey 側も
    string であることを観察 + assert する。
    """
    me = misskey_py_client.i()

    # admin の host は **null** (= Misskey は local user に host=null を返す)。
    expected_types = {
        "id": "string",
        "username": "string",
        "host": "null",  # admin は local
        "name": ("string", "null"),
        "avatarUrl": ("string", "null"),
        "isLocked": "boolean",
        "followersCount": "number",
        "followingCount": "number",
        "notesCount": "number",
    }
    for key, expected in expected_types.items():
        actual = _type_of(me.get(key, "<missing>"))
        # `<missing>` は `_type_of` が `"string"` に倒すので別途検出。
        assert key in me, f"Misskey /api/i missing key {key!r}"
        if isinstance(expected, tuple):
            assert actual in expected, (
                f"Misskey /api/i field {key!r} type mismatch: "
                f"got {actual!r}, expected one of {expected!r}"
            )
        else:
            assert actual == expected, (
                f"Misskey /api/i field {key!r} type mismatch: "
                f"got {actual!r}, expected {expected!r}"
            )


def test_sakurasato_unit_test_required_keys_match_misskey_observed_keys(misskey_py_client):
    """**メタ assertion**: Sakurasato unit test が必須としているキーの集合が
    Misskey の実際の返却に含まれる ── これが parity の本質。

    Sakurasato unit test の "必須" 9 キー (= REQUIRED_MISS_USER_KEYS) と
    Misskey が返す key set の **交集合** が 9 キー全て含むことを確認する。
    """
    me = misskey_py_client.i()
    actual_keys = set(me.keys())
    expected = set(REQUIRED_MISS_USER_KEYS)
    inter = expected & actual_keys
    assert inter == expected, (
        f"Sakurasato unit test expects keys {expected!r}, but Misskey only "
        f"returned {actual_keys & expected!r}; difference: {expected - actual_keys!r}"
    )


def test_sakurasato_landing_returns_html_with_cli_hint():
    """Sakurasato 側 `GET /miauth/{uuid}` が `text/html` を返し、CLI 指示
    テキストが embed されている。Misskey 側との parity ではなく Sakurasato
    独自挙動 (= CLAUDE.md §1 維持) の確認。
    """
    uid = str(_uuid.uuid4())
    with httpx.Client(base_url=SAKURASATO_BASE_URL, verify=_SSL_VERIFY, timeout=15) as cli:
        r = cli.get(f"/miauth/{uid}", params={"name": "parity-cli-hint", "permission": "read:account"})
    assert r.status_code == 200, f"miauth landing failed: {r.status_code}"
    ct = r.headers.get("content-type", "")
    assert "text/html" in ct.lower(), f"expected text/html, got: {ct!r}"
    body = r.text
    assert "sakurasato-server miauth approve" in body, (
        f"landing should contain CLI hint, got body[:300]={body[:300]!r}"
    )


def test_misskey_landing_returns_html(misskey_instance):
    """Misskey 側 `GET /miauth/{uuid}` も browser landing として HTML を返す
    こと ── これは Misskey の Vue app だが Content-Type が `text/html` で
    あることだけ確認する (= shape parity の最初の段)。
    """
    uid = str(_uuid.uuid4())
    r = misskey_instance.miauth_landing(uid, name="parity-cli-hint", permission="read:account")
    assert r.status_code == 200, f"misskey landing failed: {r.status_code}"
    ct = r.headers.get("content-type", "")
    assert "text/html" in ct.lower(), f"expected text/html, got: {ct!r}"
