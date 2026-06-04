#!/usr/bin/env bash
# Print connection info for the Nekonoverse 2-sks stack (#140 PR2 / Scenario A).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

cat <<'INFO'
==> Nekonoverse ↔ Sakurasato (2-sks) federation test stack
    Sakurasato (old)  https://sakurasato       (user: @me, alias to sks-new)
    Sakurasato (new)  https://sakurasato-new   (user: @me, alias to sks-old)
    Nekonoverse       https://nekonoverse      (registration open — register via UI)

    #140 PR2 (Scenario A = sks-old → sks-new Move) を実機で踏むための stack。
    手動で動かすときは以下の順:

      1. alice@sakurasato の alsoKnownAs を確認
         docker compose -f compose/docker-compose.federation-nekonoverse-2sks.yml \
             exec sakurasato-server sakurasato-server alias list
      2. alice@sakurasato-new の alsoKnownAs を確認
         docker compose -f compose/docker-compose.federation-nekonoverse-2sks.yml \
             exec sakurasato-server-new sakurasato-server alias list
      3. nekonoverse 側で bob を作る (UI / api/v1/accounts) + alice@sakurasato を follow
      4. sakurasato-server CLI で move-out
         docker compose -f compose/docker-compose.federation-nekonoverse-2sks.yml \
             exec sakurasato-server sakurasato-server move-out https://sakurasato-new/users/me
      5. bob 側で alice@sakurasato-new が follow リストに現れるのを確認

    /etc/hosts entry needed for browser access:
        127.0.0.1 sakurasato sakurasato-new nekonoverse

==> Smoke tests

    curl -sk --resolve sakurasato:443:127.0.0.1 \
      'https://sakurasato/.well-known/webfinger?resource=acct:me@sakurasato' | jq .

    curl -sk --resolve sakurasato-new:443:127.0.0.1 \
      'https://sakurasato-new/.well-known/webfinger?resource=acct:me@sakurasato-new' | jq .

==> Programmatic test (= pytest による Scenario A end-to-end):

    scripts/federation-test/pytest.sh nekonoverse-2sks

==> Logs

    docker compose -f compose/docker-compose.federation-nekonoverse-2sks.yml \
      logs -f sakurasato-server sakurasato-server-new nekonoverse-app
INFO
