#!/usr/bin/env bash
# Bootstrap the Misskey stack after up.sh.
#
# Misskey 2026.5+ defaults `meta.federation` to `'none'` (federation off).
# Federation tests require flipping it to `'all'` and restarting misskey-app
# so the in-memory meta cache picks it up. Also creates the first admin user
# (which works without auth only while no users exist) and prints its token.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

COMPOSE_FILE="compose/docker-compose.federation-misskey.yml"
ADMIN_USER="alice"
ADMIN_PASS="Password1234!"

compose() {
  docker compose -f "$COMPOSE_FILE" "$@"
}

echo "==> waiting for Misskey API to be ready"
until compose exec -T misskey-app curl -sf -X POST http://localhost:3000/api/ping \
        -H 'Content-Type: application/json' -d '{}' 2>/dev/null | grep -q pong; do
  sleep 1
done

echo "==> creating first admin user ($ADMIN_USER)"
admin_response=$(compose exec -T misskey-app curl -sf -X POST \
  http://localhost:3000/api/admin/accounts/create \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" || true)
admin_token=$(printf '%s' "$admin_response" \
  | python3 -c "import sys,json; print(json.load(sys.stdin).get('token',''))" 2>/dev/null || true)

if [ -z "$admin_token" ]; then
  echo "    (admin already exists — token not returned)"
else
  echo "    admin token: $admin_token"
fi

echo "==> enabling federation (meta.federation = 'all')"
compose exec -T postgres-mk psql -U misskey -d misskey \
  -c "UPDATE meta SET federation = 'all';" >/dev/null

echo "==> restarting misskey-app so the meta cache reloads"
compose restart misskey-app >/dev/null
until compose exec -T misskey-app curl -sf -X POST http://localhost:3000/api/ping \
        -H 'Content-Type: application/json' -d '{}' 2>/dev/null | grep -q pong; do
  sleep 1
done

cat <<INFO

==> Misskey ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me)
    Misskey     https://misskey        (admin: @$ADMIN_USER, password: $ADMIN_PASS)

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato misskey

==> Smoke tests (from inside the network — compose does not publish 443)

    docker compose -f $COMPOSE_FILE exec misskey-app \\
      curl -sk 'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato'

==> Logs

    docker compose -f $COMPOSE_FILE logs -f sakurasato-server misskey-app
INFO
