# syntax=docker/dockerfile:1.7
# Sakurasato server: distroless + rootless, musl 静的バイナリ。

# ---- builder ----
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
COPY migrations ./migrations
COPY .sqlx ./.sqlx
# `crates/core/build.rs` が `vendor/gemoji/emoji.json` を読んで Unicode emoji
# テーブルを生成する (PR #122 で導入)。server build が core を引くので必須。
COPY vendor ./vendor

# BuildKit のキャッシュマウントで registry とビルド成果物を温存。
# SQLX_OFFLINE=true で compile-time クエリ検証を .sqlx キャッシュから引く。
#
# `id=` を service ごとに分ける ── 同じ compose ビルドで server / media-proxy /
# tui 等が並列に走るときに registry cache mount (default sharing=shared) を
# 共有していると、cargo の crate 展開が race して
# `.cargo-ok File exists (os error 17)` で 1 ビルドが死ぬ。
# 別 `id=` を割り当てれば物理的に別キャッシュになるので衝突しない
# (= disk 使用量は数倍になるが、registry cache は warm でも < 500MB 程度)。
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=sakurasato-server-registry \
    --mount=type=cache,target=/build/target,id=sakurasato-server-target \
    SQLX_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-server && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-server /sakurasato-server

# 名前付き volume が初回マウントされる際の ownership を `nonroot:nonroot 0700`
# に倒すための空ディレクトリ stub。
# - `/run/sakurasato` … media-proxy との UDS (`media.sock`) を置く
# - `/run/sakurasato-local` … TUI 向けローカル API UDS (`local.sock`) を置く
# どちらも distroless/static には存在せず、何もしないと docker が
# root:root 0755 で掘ってしまい uid 65532 が bind 不能になる。
RUN mkdir -p /stub/run-sakurasato /stub/run-sakurasato-local

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-server /app/sakurasato-server
COPY --from=builder /build/config /app/config
# migrations are also embedded via sqlx::migrate! at build time, but ship
# them in the image so admins can run them manually with sqlx-cli too.
COPY --from=builder /build/migrations /app/migrations
COPY --from=builder --chown=nonroot:nonroot --chmod=0700 \
     /stub/run-sakurasato /run/sakurasato
COPY --from=builder --chown=nonroot:nonroot --chmod=0700 \
     /stub/run-sakurasato-local /run/sakurasato-local

USER nonroot:nonroot
ENV SAKURASATO_CONFIG=/app/config/default.toml
ENTRYPOINT ["/app/sakurasato-server"]
# Default to `serve`; admins can override at `docker compose run server init`.
CMD ["serve"]
