#!/usr/bin/env sh
# Nekonoverse 側に bob アカウントを headless 登録 + user-level OAuth token を
# **DB 直 seed** する (#58 / #120 PR2b)。
#
# **token bootstrap の経路選定 (PR2b)**:
#   Nekonoverse は OAuth `password` grant が無く、`authorization_code` 経路は
#   CSRF + 強制同意画面 (Nekonoverse 仕様 H-1: ログイン済みでも自動認可しない)
#   で curl 完走に最低 4 step + HTML scraping が必要。一方 Nekonoverse 自身が
#   作者本人の管理下にあり (= ユーザの過去作品) DB スキーマは安定しているので、
#   `oauth_tokens` テーブルへ直接 INSERT する経路を採用する。Mastodon stack の
#   Doorkeeper 直 seed (`tests/federation/mastodon-entrypoint.sh`) と同じ方針。
#
# フロー:
#   1. POST /api/v1/apps              ── app credentials (client_id/secret)
#   2. POST /oauth/token              ── client_credentials grant で app token
#   3. POST /api/v1/accounts          ── bob を登録
#   4. psql で oauth_tokens に直接 INSERT ── raw token (= /dev/urandom + base64) を
#      生成し、SHA-256 hash を `access_token` 列に書く (Nekonoverse の認証層
#      `backend/app/dependencies.py:147-153` は hash 検索 + plaintext fallback の
#      両対応だが、本番側と整合させるため hash 経路で揃える)。
#   5. raw token をホスト共有 volume (`/nkv-tokens/bob.token`) に書き出す。
#      pytest 側はこのパスを `NEKONOVERSE_TOKEN_FILE` 経由で読む。
#
# 出力:
#   - `${NKV_TOKEN_OUT}` (default `/nkv-tokens/bob.token`) に raw token 1 行
#
# 依存:
#   - alpine + curl + jq + postgresql-client (compose 側 image で apk add)
#   - SSL_CERT_FILE が共有テスト CA を指している
#   - Nekonoverse は `REGISTRATION_OPEN=true` で起動済み
#   - postgres-neko が同 network から `${NKV_DB_HOST}:${NKV_DB_PORT}` で引ける
set -eu

NKV_BASE="${NKV_BASE:-https://nekonoverse}"
BOB_USERNAME="${BOB_USERNAME:-bob}"
# Nekonoverse の email validator (Pydantic EmailStr 由来) は RFC 6761
# 予約 TLD (`.test` / `.example` / `.invalid` / `.local` 等) を **構文段階で**
# 拒否する。`.dev` は実 TLD (Google 運営) で予約リスト外、syntactic check
# のみ通せれば DNS 不要 (Nekonoverse 既定では deliverability check 無効)。
BOB_EMAIL="${BOB_EMAIL:-bob@bob.nekonoverse.dev}"
BOB_PASSWORD="${BOB_PASSWORD:-bobpass1234}"

# DB 直 seed 用接続情報 (compose env で上書き)。
NKV_DB_HOST="${NKV_DB_HOST:-postgres-neko}"
NKV_DB_PORT="${NKV_DB_PORT:-5432}"
NKV_DB_USER="${NKV_DB_USER:-nekonoverse}"
NKV_DB_NAME="${NKV_DB_NAME:-nekonoverse}"
NKV_DB_PASSWORD="${NKV_DB_PASSWORD:-testpass}"
# token 出力先ファイル。pytest 側 `NEKONOVERSE_TOKEN_FILE` と同じ実体を指す。
NKV_TOKEN_OUT="${NKV_TOKEN_OUT:-/nkv-tokens/bob.token}"

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
if [ "${BOB_HTTP_CODE}" = "200" ] || [ "${BOB_HTTP_CODE}" = "201" ]; then
    BOB_ACTOR_ID=$(printf '%s' "$BOB_BODY" | jq -r '.id // empty')
    if [ -z "${BOB_ACTOR_ID}" ]; then
        echo "FATAL: /api/v1/accounts ${BOB_HTTP_CODE} but missing id" >&2
        echo "       body: ${BOB_BODY}" >&2
        exit 1
    fi
    echo "==> bob account created (actor.id=${BOB_ACTOR_ID})" >&2
elif [ "${BOB_HTTP_CODE}" = "422" ] && \
     printf '%s' "$BOB_BODY" | grep -qi 'already.*in.*use'; then
    # 既存 stack 上で seed-bob を再実行した時の冪等経路 (= 同じ stack で
    # pytest を複数回回す場合 / debug で手動 retry する場合)。bob 行を作る
    # ステップだけスキップして、後続の oauth_tokens 直 seed に進む ──
    # `oauth_applications` 行は既に張られているし、別の raw token を作って
    # INSERT すれば read scope は新規分でも引き続き通る。
    echo "==> bob account already exists, skipping create (idempotent re-run)" >&2
else
    echo "FATAL: /api/v1/accounts returned HTTP ${BOB_HTTP_CODE}" >&2
    echo "       payload sent: ${BOB_PAYLOAD}" >&2
    echo "       response body: ${BOB_BODY}" >&2
    exit 1
fi

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
echo "==> webfinger OK" >&2

# Step 4: bob 用 OAuth token を DB 直 seed (PR2b)。
#
# `/dev/urandom` から 64 byte base64-ish の raw token を作り、SHA-256 hash を
# `oauth_tokens.access_token` (UNIQUE) に INSERT する。raw token 自身は
# ホスト共有 volume 配下のファイルに書き出し、pytest 側に Bearer として渡す。
#
# bob の `users.id` は `actors.username='bob' AND actors.domain IS NULL`
# (= local actor は domain NULL) → `users.actor_id` 経由で解決する。
# `oauth_applications` は 1 件想定 (`/api/v1/apps` で本 script 内で作った
# `sakurasato-e2e` のみ) なので `LIMIT 1` で十分。
echo "==> seeding oauth_token for bob via psql" >&2

# `tr -d` で base64 padding と URL-unsafe 文字を落とす。+/  → 除去、`=` → 除去。
# 結果は ascii-printable で長さ 64 (alphanumeric 中心)。Bearer header に
# そのまま貼っても shell quoting で詰まない。
RAW_TOKEN=$(head -c 48 /dev/urandom | base64 | tr -d '=+/' | head -c 64)
TOKEN_HASH=$(printf '%s' "$RAW_TOKEN" | sha256sum | awk '{print $1}')

# psql は `ON_ERROR_STOP=1` で SQL エラー時に exit 1 (= shell の `set -eu`
# とあわせて確実に止める)。`-v BOB_USERNAME` 等の psql 変数は内部で
# `:'BOB_USERNAME'` 形式で参照するとリテラル展開され、SQL injection を避けつつ
# shell の `${...}` 展開とも分離できる。
# psql は dollar-quoted body 内では `:variable` を展開しないので DO/PL/pgSQL は
# 使わず、純 SQL の CTE + `1 / count(*)::int` で 0-row 検知に倒す。
# `:'BOB_USERNAME'` / `:'TOKEN_HASH'` は CTE 本体 (dollar quote 外) で psql が
# 正しく単一引用 + escape で展開する。
#
# 動作:
#   - INSERT が 1 行以上挿入 → `1 / 1` (= 1) で SELECT 成功
#   - INSERT が 0 行           → `1 / 0` で `division by zero` → ON_ERROR_STOP=1
#     経由で psql exit 3 → set -e で script 即停止
#
# 注意: 当初 `CASE WHEN count(*) = 0 THEN 1/0 ELSE 0 END` を試したが、Postgres
# の planner が CTE INSERT を絡めた集約 + CASE の組合せで `1/0` を constant-fold
# してしまい、INSERT が成功して 1 行返したケースでも `division by zero` を
# 発火させた (psql 17 + Postgres 18 で再現)。`1 / count(*)::int` の自然形なら
# 集約値で実行時除算が回るので constant-fold されない。
PGPASSWORD="${NKV_DB_PASSWORD}" psql \
    -h "${NKV_DB_HOST}" -p "${NKV_DB_PORT}" \
    -U "${NKV_DB_USER}" -d "${NKV_DB_NAME}" \
    -v ON_ERROR_STOP=1 \
    -v BOB_USERNAME="${BOB_USERNAME}" \
    -v TOKEN_HASH="${TOKEN_HASH}" \
    <<'SQL' >&2
WITH inserted AS (
    INSERT INTO oauth_tokens
        (id, access_token, token_type, scopes,
         application_id, user_id, created_at, expires_at)
    SELECT
        gen_random_uuid(),
        :'TOKEN_HASH',
        'Bearer',
        'read write follow',
        (SELECT id FROM oauth_applications ORDER BY id LIMIT 1),
        u.id,
        now(),
        now() + interval '90 days'
    FROM users u
    JOIN actors a ON a.id = u.actor_id
    WHERE a.username = :'BOB_USERNAME' AND a.domain IS NULL
    RETURNING id
)
SELECT 1 / count(*)::int AS rows_inserted_or_die FROM inserted;
SQL

# raw token を共有 volume に書き出す。
# 出力先ディレクトリは tmpfs / named volume で compose 側が ownership を
# 倒している前提。万一無ければ mkdir で作る (= host bind の場合のフォールバック)。
mkdir -p "$(dirname "${NKV_TOKEN_OUT}")"
printf '%s\n' "${RAW_TOKEN}" > "${NKV_TOKEN_OUT}"
chmod 0644 "${NKV_TOKEN_OUT}"  # pytest user (65532) が読めるよう緩める

# Sanity: 書き出した token で実際に Bearer auth が通るか確認 (= raw → hash で
# DB の行とマッチするか)。失敗時は `verify_credentials` が 401 を返す。
echo "==> verifying bob token via verify_credentials" >&2
VC_RAW=$(
    curl -sS ${CA_FLAG} -X GET "${NKV_BASE}/api/v1/accounts/verify_credentials" \
        -H "Authorization: Bearer ${RAW_TOKEN}" \
        -w '\n%{http_code}'
)
VC_HTTP_CODE=$(printf '%s' "$VC_RAW" | tail -n1)
VC_BODY=$(printf '%s' "$VC_RAW" | sed '$d')
if [ "${VC_HTTP_CODE}" != "200" ]; then
    echo "FATAL: verify_credentials returned HTTP ${VC_HTTP_CODE}" >&2
    echo "       body: ${VC_BODY}" >&2
    exit 1
fi

echo "==> bob seeded successfully (token written to ${NKV_TOKEN_OUT})" >&2
