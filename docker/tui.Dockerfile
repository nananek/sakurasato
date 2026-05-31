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
#   # host bind の UDS (server 側 socket = `/run/sakurasato-local/local.sock` を
#   # TUI 既定の `/run/sakurasato/local.sock` にマウントし直す):
#   docker run --rm -it \
#     -e TERM \
#     -v /run/sakurasato-local:/run/sakurasato:ro \
#     -v $HOME/.config/sakurasato/token.txt:/run/secrets/token:ro \
#     ghcr.io/nananek/sakurasato-tui:latest \
#     --token-file /run/secrets/token
#
# 注意:
#   - TTY 必須 (`-it`)。`TERM` を環境変数で渡す (Kitty graphics には xterm-kitty 等)。
#   - TCP 接続時は `--network host` で tailnet IP / 公開ドメインに直接到達するのが楽。
#     ただし `--network host` は **Linux Docker 専用** ── macOS / Windows
#     (Docker Desktop) では host network namespace に入れないため、別途
#     `-p` でポート公開 + `--api-url http://host.docker.internal:...` の構成にする。
#   - TCP 例の `http://` は tailnet ACL が暗号化を担う前提。**直 IP / 公開
#     ドメイン経由では必ず HTTPS** を使うこと (TUI の Bearer トークンが
#     平文で流れないように)。
#   - UDS 接続時は server 側ソケットを TUI 既定パス (`/run/sakurasato/`) に
#     bind mount し直す。`--socket /run/sakurasato-local/local.sock` を渡す
#     代替経路もあるが、デフォルトで動く方を例示する。

# ---- builder ----
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
# sakurasato-core が `sqlx::migrate!("../../migrations")` で workspace ルートの
# migrations/ を build 時に canonicalize するため、TUI 自身は DB を叩かなくても
# builder stage には migrations/ が必要 (= server.Dockerfile と同じ理由)。
# runtime stage には migrations/ を COPY しないので image サイズは増えない。
COPY migrations ./migrations
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
