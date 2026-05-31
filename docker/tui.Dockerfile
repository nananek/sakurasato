# syntax=docker/dockerfile:1.7
# Sakurasato TUI client: distroless + rootless, musl 静的バイナリ。
#
# 使い方の例:
#
#   # tailnet 越し (PR #70 LOCAL_API_LISTEN=tcp://0.0.0.0:8443 構成):
#   docker run --rm -it \
#     -e TERM \
#     --network host \
#     -v $HOME/.config/sakurasato/token.txt:/run/secrets/token:ro \
#     ghcr.io/nananek/sakurasato-tui:latest \
#     --api-url http://sakurasato.tailnet.example.ts.net:8443 \
#     --token-file /run/secrets/token
#
#   # host bind の UDS (default `/run/sakurasato-local/local.sock`):
#   docker run --rm -it \
#     -e TERM \
#     -v /run/sakurasato-local:/run/sakurasato-local:ro \
#     -v $HOME/.config/sakurasato/token.txt:/run/secrets/token:ro \
#     ghcr.io/nananek/sakurasato-tui:latest \
#     --token-file /run/secrets/token
#
# 注意:
#   - TTY 必須 (`-it`)。`TERM` を環境変数で渡す (Kitty graphics には xterm-kitty 等)。
#   - TCP 接続時は `--network host` で tailnet IP / 公開ドメインに直接到達するのが楽。
#   - UDS 接続時はソケットを bind mount で import。

# ---- builder ----
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
COPY .sqlx ./.sqlx

# BuildKit のキャッシュマウントで registry とビルド成果物を温存。
# SQLX_OFFLINE=true は workspace 内で sqlx を使う他クレートのコンパイル時クエリ
# 検証用 (TUI 自身は DB を直接叩かないので不要だが、workspace ビルド共通の
# キャッシュを揃えるため server / media-proxy と同じ env を指定する)。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,id=sakurasato-tui-target \
    SQLX_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl -p sakurasato-tui && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-tui /sakurasato-tui

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-tui /app/sakurasato-tui

USER nonroot:nonroot
ENTRYPOINT ["/app/sakurasato-tui"]
# 引数は呼び出し側で必ず指定する (--api-url / --token-file 等)。
CMD []
