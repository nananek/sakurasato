#!/usr/bin/env bash
# Print connection info for the Nekonoverse stack.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

cat <<'INFO'
==> Nekonoverse ↔ Sakurasato federation test stack
    Sakurasato   https://sakurasato     (user: @me, RSA + Ed25519 keys)
    Nekonoverse  https://nekonoverse    (registration open — register via UI)

    Nekonoverse is sakurasato's primary Ed25519 counterpart (FEP-521a Multikey
    + dual-key). This stack is where RFC 9421 + Ed25519 against a real impl
    can actually fire end-to-end (Mastodon/Misskey/Fedibird publish RSA only).

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato nekonoverse

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve nekonoverse:443:127.0.0.1 \
      'https://nekonoverse/api/v1/instance' | jq .

==> Logs

    docker compose -f compose/docker-compose.federation-nekonoverse.yml \
      logs -f sakurasato-server nekonoverse-app
INFO
