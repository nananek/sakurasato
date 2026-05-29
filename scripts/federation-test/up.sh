#!/usr/bin/env bash
# Bring up a federation test stack for a given counterpart implementation.
#
# Usage: scripts/federation-test/up.sh <impl>
#   impl ∈ {mastodon, misskey, pleroma, mitra, fedibird, nekonoverse}
#
# Each stack is fully self-contained: sakurasato + its postgres + nginx +
# certs + the counterpart impl. CI does not run these (excluded via
# paths-ignore) — they are for manual e2e verification on a dev machine.
set -euo pipefail

IMPLS=(mastodon misskey pleroma mitra fedibird nekonoverse)

usage() {
  echo "Usage: $0 <impl>" >&2
  echo "  impl ∈ {${IMPLS[*]}}" >&2
  exit 64
}

[[ $# -eq 1 ]] || usage
impl="$1"

ok=0
for x in "${IMPLS[@]}"; do
  [[ "$x" == "$impl" ]] && ok=1 && break
done
[[ "$ok" -eq 1 ]] || usage

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

compose_file="compose/docker-compose.federation-${impl}.yml"
[[ -f "$compose_file" ]] || { echo "missing $compose_file" >&2; exit 1; }

echo "==> bringing up federation test stack: $impl"
docker compose -f "$compose_file" up -d --build --wait

echo "==> stack is up. Hosts (add to /etc/hosts if you want browser access):"
echo "    127.0.0.1 sakurasato $impl"
echo "==> follow-up: scripts/federation-test/setup-${impl}.sh"
