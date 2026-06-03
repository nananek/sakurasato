#!/usr/bin/env bash
# Run the programmatic federation pytest suite for a counterpart impl.
#
# Usage: scripts/federation-test/pytest.sh <impl>
#   impl ∈ {mastodon, nekonoverse}
#     - mastodon: httpx + AP プロトコル直叩き (M12 #56 / PR #63)
#     - nekonoverse: tmux pty 駆動 + httpx + TUI binary (M12 #58 / #120 PR2a)
#   後続 PR で misskey / pleroma / mitra / fedibird を追加していく予定。
#
# 同じ compose ファイルを **pytest プロファイル付き** で起動し、`pytest`
# サービスの exit code をそのままシェルに返す。CI からはこのスクリプトを
# wrap して使う想定。
#
# 終了時:
#   - 通常: コンテナ + volume を消す (`down -v`) → 次回 clean 起動
#   - DEBUG_KEEP=1: コンテナを残す (= ログを後から漁れる)
set -euo pipefail

IMPLS=(mastodon nekonoverse misskey)

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

project="sakurasato-fed-${impl}-pytest"

cleanup() {
  if [[ "${DEBUG_KEEP:-0}" != "1" ]]; then
    echo "==> tearing down ${project}"
    docker compose -p "$project" -f "$compose_file" --profile pytest down -v --remove-orphans \
      >/dev/null 2>&1 || true
  else
    echo "==> DEBUG_KEEP=1: leaving containers up. Inspect with:"
    echo "    docker compose -p $project -f $compose_file logs"
    echo "    docker compose -p $project -f $compose_file --profile pytest down -v"
  fi
}
trap cleanup EXIT

echo "==> building ${impl} federation pytest stack (project=${project})"
# `docker compose run` は対象サービスの `depends_on` チェーンを自動で起動
# しつつ healthcheck / `service_completed_successfully` を待つ。`up` の
# `--abort-on-container-exit` だと certs や sakurasato-init / token-issuer
# 等の **正常終了した one-shot** がトリガになりスタック全体を abort して
# しまうので、`run` で pytest だけを前景で走らせ、その終了コードを直接
# シェルに返す方が素直。
docker compose -p "$project" -f "$compose_file" --profile pytest build
docker compose -p "$project" -f "$compose_file" --profile pytest run --rm pytest
