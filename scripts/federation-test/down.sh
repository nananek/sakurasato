#!/usr/bin/env bash
# Tear down a federation test stack started by up.sh.
#
# Usage: scripts/federation-test/down.sh <impl>
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

echo "==> tearing down: $impl"
docker compose -f "$compose_file" down -v
