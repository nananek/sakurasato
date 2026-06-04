#!/usr/bin/env sh
# bob (nkv) から alice@sakurasato (sks-old) への Follow を投入し、accepted まで
# polling する setup 1-shot (#140 PR2 / Scenario A)。
#
# 入力 env:
#   NKV_BASE           ── 例: https://nekonoverse
#   NKV_TOKEN_FILE     ── bob の raw Bearer (= nekonoverse-bob-issuer が書く)
#   ALICE_OLD_AP_ID    ── 例: https://sakurasato/users/me
#
# フロー:
#   1. Bearer を読む
#   2. nkv `/api/v2/search?q=<ALICE_OLD_AP_ID>&resolve=true&type=accounts`
#      で alice の nkv-local account id を引く
#   3. `/api/v1/accounts/{id}/follow` を POST
#   4. `/api/v1/accounts/relationships?id[]={id}` を polling し
#      `.[0].following == true` になるまで待つ (= accepted 確認)
#
# Follow が accepted になる条件 = sks-old が `auto-Accept` で返した Accept
# activity が nkv に届くこと。sks-old default は auto-Accept なので、bob の
# follow は 30s 程度で accepted に倒れる。
set -eu

BOB_TOKEN=$(cat "${NKV_TOKEN_FILE}")
[ -n "${BOB_TOKEN}" ] || {
    echo "FATAL: empty bob token file ${NKV_TOKEN_FILE}" >&2
    exit 1
}

echo "==> resolving ${ALICE_OLD_AP_ID} on ${NKV_BASE}" >&2

# `--data-urlencode` を使うと URL-safe な ? の組み立てが楽。`-G` で GET にする。
SEARCH_RAW=""
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    SEARCH_RAW=$(
        curl -fsS -G \
            "${NKV_BASE}/api/v2/search" \
            --data-urlencode "q=${ALICE_OLD_AP_ID}" \
            --data-urlencode "resolve=true" \
            --data-urlencode "type=accounts" \
            -H "Authorization: Bearer ${BOB_TOKEN}" \
            2>/dev/null || true
    )
    ALICE_ID=$(printf '%s' "${SEARCH_RAW}" | jq -r '.accounts[0].id // empty' 2>/dev/null || true)
    if [ -n "${ALICE_ID}" ]; then
        break
    fi
    echo "==> attempt ${attempt}: search did not resolve yet; sleeping 3s" >&2
    sleep 3
done

if [ -z "${ALICE_ID}" ]; then
    echo "FATAL: could not resolve ${ALICE_OLD_AP_ID} via nkv search" >&2
    echo "       last raw response: ${SEARCH_RAW}" >&2
    exit 1
fi
echo "==> alice nkv-local account id = ${ALICE_ID}" >&2

# Follow を POST。Mastodon spec で 200 + relationship JSON 返し。
echo "==> posting follow" >&2
curl -fsS -X POST \
    "${NKV_BASE}/api/v1/accounts/${ALICE_ID}/follow" \
    -H "Authorization: Bearer ${BOB_TOKEN}" \
    >/dev/null

# accepted まで polling。
# sks-old default = auto-Accept、平均 5-10s で立つ。timeout 120s。
echo "==> waiting for follow to become accepted on nkv side" >&2
for attempt in $(seq 1 40); do
    REL=$(
        curl -fsS -G \
            "${NKV_BASE}/api/v1/accounts/relationships" \
            --data-urlencode "id[]=${ALICE_ID}" \
            -H "Authorization: Bearer ${BOB_TOKEN}" \
            2>/dev/null || true
    )
    FOLLOWING=$(printf '%s' "${REL}" | jq -r '.[0].following // false' 2>/dev/null || echo "false")
    if [ "${FOLLOWING}" = "true" ]; then
        echo "==> follow accepted after ${attempt} attempts" >&2
        exit 0
    fi
    sleep 3
done

echo "FATAL: follow to ${ALICE_OLD_AP_ID} did not become accepted within 120s" >&2
echo "       last relationships response: ${REL}" >&2
exit 1
