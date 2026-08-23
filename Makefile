# Sakurasato dev タスク。ビルドは常に Docker 内で行う (§9)。
# 実体は scripts/dev/cargo.sh (rust:1.96-alpine dev イメージで cargo を実行)。
#
#   make build   … cargo build --workspace --locked
#   make clippy  … cargo clippy --workspace --all-targets --locked -- -D warnings
#   make test    … cargo test --workspace --locked
#   make fmt     … cargo fmt --all
#   make check   … fmt --check + clippy + test (CI と同じ 3 点セット)
#   make cargo ARGS='tree --locked -i crossterm'  … 任意の cargo 実行

CARGO := ./scripts/dev/cargo.sh

.PHONY: build clippy test fmt fmt-check check cargo

build:
	$(CARGO) build --workspace --locked

clippy:
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

test:
	$(CARGO) test --workspace --locked

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

# CI (ci.yml) と同じゲート: fmt --check → clippy -D warnings → test。
check: fmt-check clippy test

# 任意 cargo: make cargo ARGS='tree --locked -i crossterm'
cargo:
	$(CARGO) $(ARGS)
