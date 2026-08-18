#!/bin/sh
# Pleroma entrypoint for federation tests — trusts the shared test CA so
# Pleroma's outbound TLS to https://sakurasato/ works, then creates the
# `bob` test account via /app/cli.sh (= mix pleroma.user new のラッパー)。
# 参照: tmp/plan-federation-test-pleroma-mitra-fedibird.md §2.A,
#       tmp/plan-pleroma-bob-account-fix.md。
#
# `ghcr.io/explodingcamera/pleroma:stable` は OTP release ではなくソース
# ツリーそのものを同梱する構成 (`mix phx.server` で起動)。そのため
# `pleroma_ctl` 経由のリリースバイナリ呼び出しは使えない
# (`/app/rel/files/bin/pleroma_ctl` は release テンプレートの残骸で、
# 隣接する `pleroma` 実体バイナリが存在しない)。イメージ同梱の固定パス
# `/app/cli.sh` (`su-exec pleroma mix pleroma.$@`) を使う。
set -e

if [ -f /certs/ca.crt ]; then
  cp /certs/ca.crt /usr/local/share/ca-certificates/test-federation-ca.crt
  update-ca-certificates 2>/dev/null
  echo "Added test CA cert to trust store"
fi

BOB_USERNAME="${BOB_USERNAME:-bob}"
BOB_EMAIL="${BOB_EMAIL:-bob@pleroma}"
BOB_PASSWORD="${BOB_PASSWORD:-Password1234!}"
BOB_DOMAIN="${DOMAIN:-pleroma}"

/app/start.sh &
PLEROMA_PID=$!

http_ready() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsS -o /dev/null "$url" 2>/dev/null
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O /dev/null "$url" 2>/dev/null
  else
    return 1
  fi
}

echo "Waiting for Pleroma HTTP to become ready..."
i=0
until http_ready "http://127.0.0.1:4000/api/v1/instance"; do
  i=$((i + 1))
  if [ "$i" -ge 120 ]; then
    echo "ERROR: Pleroma did not answer /api/v1/instance within 120s" >&2
    exit 1
  fi
  sleep 1
done
echo "Pleroma HTTP check done (waited ${i}s)"

webfinger_ok() {
  http_ready "http://127.0.0.1:4000/.well-known/webfinger?resource=acct:${BOB_USERNAME}@${BOB_DOMAIN}"
}

if webfinger_ok; then
  echo "${BOB_USERNAME}@${BOB_DOMAIN} already resolvable; skipping account creation"
elif [ -x /app/cli.sh ]; then
  echo "Creating ${BOB_USERNAME} account via /app/cli.sh..."
  /app/cli.sh user new "$BOB_USERNAME" "$BOB_EMAIL" \
    --password "$BOB_PASSWORD" -y 2>&1 || true
  if ! webfinger_ok; then
    echo "ERROR: ${BOB_USERNAME} account creation failed (webfinger still unresolvable after /app/cli.sh attempt)" >&2
    exit 1
  fi
  echo "${BOB_USERNAME} account created"
else
  echo "ERROR: /app/cli.sh not found; cannot create ${BOB_USERNAME} account" >&2
  exit 1
fi

# compose の healthcheck (pleroma-app) が見るマーカー。実際に bob が
# webfinger で解決可能になったことを確認した後にのみ書く (以前は
# pleroma_ctl/cli.sh の成否を無視して無条件 touch していたため、アカウント
# 作成失敗時も healthy 扱いになり、失敗の症状が後続の
# sakurasato-prefollow-bob (WebFinger 404 → media-proxy 502) に転嫁して
# 観測されていた)。
touch /tmp/sakurasato-bob-ready
echo "bob account setup complete; wrote readiness marker"

wait "$PLEROMA_PID"
