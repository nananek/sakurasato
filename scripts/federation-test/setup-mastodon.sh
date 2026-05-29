#!/usr/bin/env bash
# Print connection info and a smoke-test curl for the Mastodon stack.
# Sakurasato (init + serve) and Mastodon (entrypoint creates `bob`) are
# already bootstrapped by `up.sh`; this script just shows what to do next.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

compose_file="compose/docker-compose.federation-mastodon.yml"

cat <<'INFO'
==> Mastodon ↔ Sakurasato federation test stack
    Sakurasato  https://sakurasato     (user: @me)
    Mastodon    https://mastodon       (user: @bob, password: Password1234!)

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato mastodon

==> Smoke tests (run from host)

    # WebFinger from sakurasato side
    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    # actor JSON
    curl -sk --resolve sakurasato:443:127.0.0.1 \
      -H 'Accept: application/activity+json' \
      'https://sakurasato/users/me' | jq .

==> From inside the network (sakurasato signature verification path)

    docker compose -f compose/docker-compose.federation-mastodon.yml \
      exec mastodon-web bash -c \
      "curl -sk -H 'Accept: application/activity+json' https://sakurasato/users/me"

==> Logs to watch for HTTP signature verification

    docker compose -f compose/docker-compose.federation-mastodon.yml \
      logs -f sakurasato-server
INFO
