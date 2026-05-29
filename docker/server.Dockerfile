# syntax=docker/dockerfile:1.7
# Sakurasato server: distroless + rootless, musl 静的バイナリ。

# ---- builder ----
FROM rust:1.95-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config

# BuildKit のキャッシュマウントで registry とビルド成果物を温存。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,id=sakurasato-server-target \
    cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-server && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-server /sakurasato-server

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-server /app/sakurasato-server
COPY --from=builder /build/config /app/config

USER nonroot:nonroot
ENV SAKURASATO_CONFIG=/app/config/default.toml
ENTRYPOINT ["/app/sakurasato-server"]
