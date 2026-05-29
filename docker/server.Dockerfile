# syntax=docker/dockerfile:1.7
# Sakurasato server: distroless + rootless, musl 静的バイナリ。

# ---- builder ----
FROM rust:1.95-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
COPY migrations ./migrations
COPY .sqlx ./.sqlx

# BuildKit のキャッシュマウントで registry とビルド成果物を温存。
# SQLX_OFFLINE=true で compile-time クエリ検証を .sqlx キャッシュから引く。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,id=sakurasato-server-target \
    SQLX_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-server && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-server /sakurasato-server

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-server /app/sakurasato-server
COPY --from=builder /build/config /app/config
# migrations are also embedded via sqlx::migrate! at build time, but ship
# them in the image so admins can run them manually with sqlx-cli too.
COPY --from=builder /build/migrations /app/migrations

USER nonroot:nonroot
ENV SAKURASATO_CONFIG=/app/config/default.toml
ENTRYPOINT ["/app/sakurasato-server"]
# Default to `serve`; admins can override at `docker compose run server init`.
CMD ["serve"]
