#!/usr/bin/env bash
# Print connection info for the Pleroma stack.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

cat <<'INFO'
==> Pleroma ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me)
    Pleroma     https://pleroma        (registrations are open — register via UI)

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato pleroma

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve pleroma:443:127.0.0.1 \
      'https://pleroma/api/v1/instance' | jq .

==> Logs

    docker compose -f compose/docker-compose.federation-pleroma.yml \
      logs -f sakurasato-server pleroma-app
INFO
