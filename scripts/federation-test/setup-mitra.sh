#!/usr/bin/env bash
# Print connection info for the Mitra stack.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

cat <<'INFO'
==> Mitra ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me, RSA + Ed25519 keys)
    Mitra       https://mitra          (user: @bob, password: password123)

    Mitra is one of the few impls that exposes FEP-521a Multikey, so this
    stack is the right place to exercise the RFC 9421 + Ed25519 inbound
    signature path on sakurasato.

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato mitra

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve mitra:443:127.0.0.1 \
      'https://mitra/api/v1/instance' | jq .

==> Logs

    docker compose -f compose/docker-compose.federation-mitra.yml \
      logs -f sakurasato-server mitra-app
INFO
