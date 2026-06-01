#!/usr/bin/env sh
# Nekonoverse 側に bob アカウントを headless 登録する (#58 / #120 PR2a)。
#
# **スコープ (PR2a)**: bob を作るところまで。bob の access token は **取らない**。
# Nekonoverse は OAuth で `authorization_code` と `client_credentials` のみを
# サポートし `password` grant が無いため、headless で user-level token を得る
# には authorize form scraping か DB 直 seed が必要。PR2a smoke は sks 側の
# `following()` 一覧で「sks → nkv Follow round-trip が accepted まで通った」
# ことを確認すれば足りるので、nkv 側 follower 一覧の verify は PR2b に送る。
#
# フロー:
#   1. POST /api/v1/apps              ── app credentials (client_id/secret)
#   2. POST /oauth/token              ── client_credentials grant で app token
#   3. POST /api/v1/accounts          ── bob を登録 (= Account を作るだけ)
#
# 出力:
#   なし (bob の存在が WebFinger で引けるようになることが副作用)。
#
# 依存:
#   - alpine:latest + curl + jq (compose 側 image で apk add する)
#   - SSL_CERT_FILE が共有テスト CA を指している (sakurasato → nkv の TLS と同経路)
#   - Nekonoverse は `REGISTRATION_OPEN=true` で起動済み
set -eu

NKV_BASE="${NKV_BASE:-https://nekonoverse}"
BOB_USERNAME="${BOB_USERNAME:-bob}"
# Nekonoverse の email validator (Pydantic EmailStr 由来) は RFC 6761
# 予約 TLD (`.test` / `.example` / `.invalid` / `.local` 等) を **構文段階で**
# 拒否する。`.dev` は実 TLD (Google 運営) で予約リスト外、syntactic check
# のみ通せれば DNS 不要 (Nekonoverse 既定では deliverability check 無効)。
BOB_EMAIL="${BOB_EMAIL:-bob@bob.nekonoverse.dev}"
BOB_PASSWORD="${BOB_PASSWORD:-bobpass1234}"

echo "==> nekonoverse seed: ${NKV_BASE} (bob account only, no token)" >&2

# CA を信頼。SSL_CERT_FILE で update-ca-certificates した経路と揃える。
# curl の `--cacert` は SSL_CERT_FILE を見ない場合があるので明示する。
CA_FLAG=""
if [ -n "${SSL_CERT_FILE:-}" ] && [ -f "${SSL_CERT_FILE}" ]; then
    CA_FLAG="--cacert ${SSL_CERT_FILE}"
fi

# Step 1: register app.
APP_RESP=$(
    curl -fsS ${CA_FLAG} -X POST "${NKV_BASE}/api/v1/apps" \
        -H "Content-Type: application/json" \
        -d '{"client_name":"sakurasato-e2e","redirect_uris":"urn:ietf:wg:oauth:2.0:oob","scopes":"read write follow"}'
)
CLIENT_ID=$(printf '%s' "$APP_RESP" | jq -r '.client_id // empty')
CLIENT_SECRET=$(printf '%s' "$APP_RESP" | jq -r '.client_secret // empty')
if [ -z "${CLIENT_ID}" ] || [ -z "${CLIENT_SECRET}" ]; then
    echo "FATAL: /api/v1/apps did not return client_id/secret: ${APP_RESP}" >&2
    exit 1
fi
echo "==> got app credentials" >&2

# Step 2: app token (client_credentials).
TOKEN_RESP=$(
    curl -fsS ${CA_FLAG} -X POST "${NKV_BASE}/oauth/token" \
        -H "Content-Type: application/json" \
        -d "$(printf '{"grant_type":"client_credentials","client_id":"%s","client_secret":"%s","scope":"read write follow"}' "${CLIENT_ID}" "${CLIENT_SECRET}")"
)
APP_TOKEN=$(printf '%s' "$TOKEN_RESP" | jq -r '.access_token // empty')
if [ -z "${APP_TOKEN}" ]; then
    echo "FATAL: /oauth/token did not return access_token: ${TOKEN_RESP}" >&2
    exit 1
fi
echo "==> got app token" >&2

# Step 3: register bob. Mastodon 互換の Token object を返す想定
# (`{access_token, token_type, scope, created_at}`)。
#
# 失敗時のデバッグのため `--fail` を使わず、HTTP code + body を一緒に拾う:
#   - `-w '\n%{http_code}'` で body の末尾に改行 + status code を貼る
#   - 200 / 201 以外なら body をそのまま stderr に出して exit 1
# Nekonoverse が Mastodon 互換 と異なる field を要求するケース (例:
# `locale` 不要 / `agreement` 不要) は body のエラーメッセージで判別可能。
BOB_PAYLOAD=$(printf '{"username":"%s","email":"%s","password":"%s","agreement":true,"locale":"en"}' \
    "${BOB_USERNAME}" "${BOB_EMAIL}" "${BOB_PASSWORD}")
BOB_RAW=$(
    curl -sS ${CA_FLAG} -X POST "${NKV_BASE}/api/v1/accounts" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${APP_TOKEN}" \
        -d "${BOB_PAYLOAD}" \
        -w '\n%{http_code}'
)
BOB_HTTP_CODE=$(printf '%s' "$BOB_RAW" | tail -n1)
BOB_BODY=$(printf '%s' "$BOB_RAW" | sed '$d')
if [ "${BOB_HTTP_CODE}" != "200" ] && [ "${BOB_HTTP_CODE}" != "201" ]; then
    echo "FATAL: /api/v1/accounts returned HTTP ${BOB_HTTP_CODE}" >&2
    echo "       payload sent: ${BOB_PAYLOAD}" >&2
    echo "       response body: ${BOB_BODY}" >&2
    exit 1
fi
# PR2a スコープ: bob の access token は取らない (= user-level token を得るには
# OAuth authorization_code dance か DB 直 seed が必要で、本 PR の範囲外)。
# sks 側の `/api/v1/following` で「sks → nkv の Follow が accepted まで
# 通った」ことを確認する経路に倒すので、bob の存在 (= WebFinger で引ける) が
# 担保できれば足りる。PR2b で OAuth dance を組み込んで nkv side の verify
# (followers 一覧 / post / reaction 等) を追加する予定。
BOB_ID=$(printf '%s' "$BOB_BODY" | jq -r '.id // empty')
if [ -z "${BOB_ID}" ]; then
    echo "FATAL: /api/v1/accounts ${BOB_HTTP_CODE} but missing id" >&2
    echo "       body: ${BOB_BODY}" >&2
    exit 1
fi
echo "==> bob account created (id=${BOB_ID})" >&2

# Sanity: WebFinger で bob が引けることを確認 (= sks 側からも見える)。
# host header を `nekonoverse` で固定する。
echo "==> verifying webfinger lookup for bob" >&2
WF_RAW=$(
    curl -sS ${CA_FLAG} -X GET \
        "${NKV_BASE}/.well-known/webfinger?resource=acct:${BOB_USERNAME}@nekonoverse" \
        -w '\n%{http_code}'
)
WF_HTTP_CODE=$(printf '%s' "$WF_RAW" | tail -n1)
WF_BODY=$(printf '%s' "$WF_RAW" | sed '$d')
if [ "${WF_HTTP_CODE}" != "200" ]; then
    echo "FATAL: WebFinger lookup returned HTTP ${WF_HTTP_CODE}" >&2
    echo "       body: ${WF_BODY}" >&2
    exit 1
fi
echo "==> bob seeded successfully (webfinger OK)" >&2
