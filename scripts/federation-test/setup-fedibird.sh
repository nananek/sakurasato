#!/usr/bin/env bash
# Print connection info for the Fedibird stack.
# The entrypoint writes bob's OAuth token to a docker volume (fedibird_tokens);
# extract it here for convenience.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

compose_file="compose/docker-compose.federation-fedibird.yml"

cat <<'INFO'
==> Fedibird ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me)
    Fedibird    https://fedibird       (user: @bob, password: Password1234!)

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato fedibird
INFO

token="$(docker compose -f "$compose_file" exec -T fedibird-web cat /tokens/bob_token.txt 2>/dev/null || true)"
if [[ -n "$token" ]]; then
  echo
  echo "==> bob's OAuth access token (Fedibird 3.4.1 has no password grant):"
  echo "    $token"
fi

cat <<'INFO'

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve fedibird:443:127.0.0.1 \
      'https://fedibird/api/v1/instance' | jq .

==> Logs

    docker compose -f compose/docker-compose.federation-fedibird.yml \
      logs -f sakurasato-server fedibird-web
INFO
