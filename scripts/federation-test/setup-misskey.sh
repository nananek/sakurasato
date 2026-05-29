#!/usr/bin/env bash
# Print connection info for the Misskey stack.
# Note: Misskey's initial admin account is created via the Misskey UI on
# first access — there is no headless bootstrap for it.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

cat <<'INFO'
==> Misskey ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me)
    Misskey     https://misskey        (create admin via web UI on first access)

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato misskey

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve misskey:443:127.0.0.1 \
      'https://misskey/api/meta' -X POST -H 'Content-Type: application/json' -d '{}' | jq .

==> Logs

    docker compose -f compose/docker-compose.federation-misskey.yml \
      logs -f sakurasato-server misskey-app
INFO
