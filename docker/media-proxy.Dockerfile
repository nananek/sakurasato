# syntax=docker/dockerfile:1.7
# Sakurasato media-proxy: 隔離コンテナ。distroless + rootless、musl 静的バイナリ。

# ---- builder ----
FROM rust:1.95-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,id=sakurasato-media-proxy-target \
    cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-media-proxy && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-media-proxy /sakurasato-media-proxy

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-media-proxy /app/sakurasato-media-proxy
COPY --from=builder /build/config /app/config

USER nonroot:nonroot
ENV SAKURASATO_CONFIG=/app/config/default.toml
ENTRYPOINT ["/app/sakurasato-media-proxy"]
