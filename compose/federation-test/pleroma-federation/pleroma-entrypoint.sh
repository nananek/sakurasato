#!/bin/sh
# Pleroma entrypoint for federation tests — trusts the shared test CA so
# Pleroma's outbound TLS to https://sakurasato/ works, then creates the
# `bob` test account (pytest 側は `/api/v1/apps` + `/oauth/token` の
# password grant で token を取得する ── Pleroma は Mastodon と異なり
# password grant を引き続き提供するため、Mitra と同じ経路を踏襲できる。
# 参照: tmp/plan-federation-test-pleroma-mitra-fedibird.md §2.A)。
#
# `ghcr.io/explodingcamera/pleroma:stable` の内部レイアウト (pleroma_ctl の
# 配置場所) は実機未検証のため、複数の候補パスを探索するディフェンシブな
# 実装にしてある。CI (federation-test-pleroma.yml) の実行結果を見て、
# 検出に失敗するようなら候補パスを追記すること。
set -e

if [ -f /certs/ca.crt ]; then
  cp /certs/ca.crt /usr/local/share/ca-certificates/test-federation-ca.crt
  update-ca-certificates 2>/dev/null
  echo "Added test CA cert to trust store"
fi

BOB_USERNAME="${BOB_USERNAME:-bob}"
BOB_EMAIL="${BOB_EMAIL:-bob@pleroma}"
BOB_PASSWORD="${BOB_PASSWORD:-Password1234!}"

# Pleroma 本体をバックグラウンドで起動し、HTTP が応答するまで待ってから
# ユーザー作成 CLI を叩く (migration 完了前に叩くと失敗するため)。
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
    echo "WARNING: Pleroma did not answer /api/v1/instance within 120s; proceeding anyway" >&2
    break
  fi
  sleep 1
done
echo "Pleroma HTTP check done (waited ${i}s)"

# pleroma_ctl (OTP release CLI) の実体パスを候補から探す。見つからなければ
# PATH、それでも無ければファイルシステム探索にフォールバックする。
PLEROMA_CTL=""
for candidate in /app/bin/pleroma_ctl /opt/pleroma/bin/pleroma_ctl /app/release/bin/pleroma_ctl; do
  if [ -x "$candidate" ]; then
    PLEROMA_CTL="$candidate"
    break
  fi
done
if [ -z "$PLEROMA_CTL" ] && command -v pleroma_ctl >/dev/null 2>&1; then
  PLEROMA_CTL="$(command -v pleroma_ctl)"
fi
if [ -z "$PLEROMA_CTL" ]; then
  PLEROMA_CTL="$(find / -xdev -maxdepth 6 -name pleroma_ctl -type f 2>/dev/null | head -n1)"
fi

if [ -n "$PLEROMA_CTL" ]; then
  echo "Creating ${BOB_USERNAME} account via ${PLEROMA_CTL}..."
  "$PLEROMA_CTL" user new "$BOB_USERNAME" "$BOB_EMAIL" \
    --password "$BOB_PASSWORD" --assume-yes 2>&1 \
    || "$PLEROMA_CTL" user new "$BOB_USERNAME" "$BOB_EMAIL" \
      --password "$BOB_PASSWORD" -y 2>&1 \
    || echo "WARNING: pleroma_ctl user new failed (maybe ${BOB_USERNAME} already exists)"
else
  echo "ERROR: pleroma_ctl not found anywhere under /; cannot create ${BOB_USERNAME} account" >&2
  exit 1
fi

# compose の healthcheck (pleroma-app) が見るマーカー。HTTP は bob 作成より
# 先に応答してしまう (start.sh を先にバックグラウンド起動しているため) ので、
# `/api/v1/instance` 疎通だけでは pytest 側の `sakurasato-prefollow-bob` /
# `pytest` サービスが bob 未作成のまま先行してしまうレースがある。bob 作成が
# 完了した (= このスクリプトのここまでの処理が終わった) 時点でマーカーを書き、
# healthcheck 側はこのファイルの存在だけを見る。
touch /tmp/sakurasato-bob-ready
echo "bob account setup complete; wrote readiness marker"

wait "$PLEROMA_PID"
