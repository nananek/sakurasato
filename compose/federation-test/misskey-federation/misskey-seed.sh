#!/usr/bin/env sh
# M14 #162: Misskey の admin user を作って `i` token を共有 volume に書く。
#
# Misskey の `POST /api/admin/accounts/create` は **未認証** で叩けるのが
# 「ユーザがまだ 1 人も居ない間」だけ ── 初回 boot 後 1 回だけ走らせて admin
# を作り、レスポンスの `token` (= 個人 API トークン = `i` で叩ける Bearer
# 相当) を `/misskey-tokens/admin.token` に書き出す。
#
# ## 認証なし API 呼び出しの根拠
#
# Misskey 公式 OpenAPI ([api-doc.misskey.io](https://api-doc.misskey.io/)) で
# `admin/accounts/create` は **初回のみ unauthenticated で叩ける** と明記。
# 2 人目以降の admin 作成は既存 admin の token が必要。本 seed は単一 instance
# 内のテスト用 admin を 1 人だけ作るのでこの抜け穴で十分。
#
# ## AGPL discipline
#
# 本スクリプトは公開 API 仕様のみを参照 ── Misskey の TypeScript source は
# 読んでいない。観察ベースのコマンド呼び出しのみ ([[agpl-discipline-miauth]])。
#
# ## 冪等性
#
# 既に token ファイルが存在する場合はスキップする (= 多重起動防御)。compose
# `restart: "no"` で 1 回しか走らない設計だが、`down -v` 忘れの開発フローで
# stale volume が残った時の保護として。
set -eu

MISSKEY_BASE="${MISSKEY_BASE:-http://misskey-app:3000}"
ADMIN_USERNAME="${ADMIN_USERNAME:-admin}"
ADMIN_PASSWORD="${ADMIN_PASSWORD:-AdminTestPass1234}"
ADMIN_TOKEN_OUT="${ADMIN_TOKEN_OUT:-/misskey-tokens/admin.token}"

echo "==> misskey seed: target=${MISSKEY_BASE} user=${ADMIN_USERNAME}" >&2

# Volume が消えてない場合の冪等 short-circuit。
if [ -s "${ADMIN_TOKEN_OUT}" ]; then
    echo "==> ${ADMIN_TOKEN_OUT} already exists, skipping" >&2
    exit 0
fi

# `--http1.1` を使うのは Misskey のアップストリームが HTTP/2 で意外な挙動を
# することがあるため。テスト経路は明示的に 1.1 で固定。
CURL="curl --silent --show-error --fail --http1.1"

# Misskey の `/api/ping` が 200 を返すまで polling。compose の healthcheck で
# 待ち合わせているが、念のため seed 内でも 90s リトライする (= healthcheck の
# `interval` の谷間で start_period 外れる事故対策)。
i=0
until $CURL -X POST -H 'Content-Type: application/json' \
       --data '{}' "${MISSKEY_BASE}/api/ping" 2>/dev/null | grep -q pong; do
    i=$((i + 1))
    if [ "$i" -gt 30 ]; then
        echo "==> misskey /api/ping never became ready" >&2
        exit 1
    fi
    sleep 3
done

echo "==> creating first admin via /api/admin/accounts/create" >&2

# 初回 admin 作成は **未認証 POST** で OK。User-Agent はオプションだが指定して
# おくと misskey 側のレートリミットがゆるい。
admin_resp=$($CURL -X POST \
    -H 'Content-Type: application/json' \
    -H 'User-Agent: sakurasato-federation-test/1.0' \
    --data "{\"username\":\"${ADMIN_USERNAME}\",\"password\":\"${ADMIN_PASSWORD}\"}" \
    "${MISSKEY_BASE}/api/admin/accounts/create" 2>&1) || {
    echo "==> admin creation failed:" >&2
    echo "${admin_resp}" >&2
    exit 1
}

# レスポンスの `token` フィールドを取り出す。jq が見つからない fallback として
# python3 を試す ── 本 image (alpine + apk add curl jq) では jq が確実に居る
# が、別 image にコピペで使われた時の保険。
token=$(printf '%s' "${admin_resp}" | jq -r '.token // empty' 2>/dev/null || true)
if [ -z "${token}" ]; then
    token=$(printf '%s' "${admin_resp}" \
        | python3 -c "import sys,json; print(json.load(sys.stdin).get('token','') or '')" \
        2>/dev/null || true)
fi

if [ -z "${token}" ]; then
    echo "==> admin response did not contain a token:" >&2
    echo "${admin_resp}" >&2
    exit 1
fi

# token ファイルを 0o600 で書く ── pytest 側 user (= 65532) が読めるよう
# /misskey-tokens は 0o755 で見える前提 (= named volume の初期 perms)。
mkdir -p "$(dirname "${ADMIN_TOKEN_OUT}")"
printf '%s\n' "${token}" > "${ADMIN_TOKEN_OUT}"
chmod 0644 "${ADMIN_TOKEN_OUT}"

echo "==> wrote admin token to ${ADMIN_TOKEN_OUT} (length=${#token})" >&2
