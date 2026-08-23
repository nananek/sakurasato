#!/usr/bin/env bash
# Docker 内で cargo を実行する dev ラッパ。ホストに rust を入れず
# 「ビルドは常に Docker」で完結させるためのもの (§9)。
#
# 使い方:
#   scripts/dev/cargo.sh build --workspace --locked
#   scripts/dev/cargo.sh clippy --workspace --all-targets --locked -- -D warnings
#   scripts/dev/cargo.sh test --workspace --locked
#   scripts/dev/cargo.sh fmt --all -- --check
#
# 設計メモ:
# - toolchain は docker/dev.Dockerfile (= release と同じ rust:1.96-alpine) に固定。
# - workspace を /build に bind mount し、cargo をそこで走らせる。
# - `--user $(id -u):$(id -g)` で **ホスト側に root 所有ファイルを作らない**。
#   そのため CARGO_HOME (registry/index) と target を、ホストの XDG cache 配下
#   (= 起動ユーザ所有) に bind mount する。名前付き volume は root 所有で作られ
#   非 root 実行だと書けないため、あえてホスト bind dir を使う。
# - RUSTUP_HOME はイメージ内 (/usr/local/rustup, 全ユーザ読取可) をそのまま使う。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="sakurasato-dev:local"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/sakurasato-dev"

mkdir -p "$CACHE/cargo" "$CACHE/target"

# dev イメージを (無ければ / Dockerfile が変わっていれば) ビルド。BuildKit の
# レイヤキャッシュが効くので 2 回目以降は数百 ms。
DOCKER_BUILDKIT=1 docker build -q \
  -f "$ROOT/docker/dev.Dockerfile" \
  -t "$IMAGE" "$ROOT" >/dev/null

# 対話端末があるときだけ -it を付ける (CI / パイプでは付けない)。
TTY_FLAGS=()
if [ -t 0 ] && [ -t 1 ]; then
  TTY_FLAGS=(-it)
fi

exec docker run --rm "${TTY_FLAGS[@]}" \
  --user "$(id -u):$(id -g)" \
  -v "$ROOT":/build \
  -v "$CACHE/cargo":/cargo \
  -v "$CACHE/target":/build/target \
  -e CARGO_HOME=/cargo \
  -w /build \
  "$IMAGE" \
  cargo "$@"
