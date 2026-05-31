# syntax=docker/dockerfile:1.7
# Sakurasato media-proxy: 隔離コンテナ。distroless + rootless、musl 静的バイナリ。

# ---- builder ----
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
COPY migrations ./migrations
COPY .sqlx ./.sqlx

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,id=sakurasato-media-proxy-target \
    SQLX_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-media-proxy && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-media-proxy /sakurasato-media-proxy

# /run/sakurasato を `nonroot:nonroot 0700` で先に掘っておく。これにより
# media_sock 名前付き volume の初回マウント時 docker がこの ownership を
# そのまま引き継ぐ (= runtime user uid 65532 が socket を bind 可能)。
# 掘る場所だけ用意できればよいので空ディレクトリ。`--chmod=0700` 付き
# `COPY` で nonroot 専用にする。
RUN mkdir -p /stub/run-sakurasato

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-media-proxy /app/sakurasato-media-proxy
COPY --from=builder /build/config /app/config
COPY --from=builder --chown=nonroot:nonroot --chmod=0700 \
     /stub/run-sakurasato /run/sakurasato

USER nonroot:nonroot
ENV SAKURASATO_CONFIG=/app/config/default.toml
ENTRYPOINT ["/app/sakurasato-media-proxy"]
