# syntax=docker/dockerfile:1.7
# Sakurasato TUI client: distroless + rootless, musl 静的バイナリ。
#
# 使い方の例:
#
#   # tailnet 越し (PR #70 LOCAL_API_LISTEN=tcp://0.0.0.0:8443 構成):
#   docker run --rm -it \
#     --cap-drop=ALL --security-opt=no-new-privileges:true --read-only \
#     --tmpfs /tmp \
#     -e TERM \
#     --network host \
#     -v $HOME/.config/sakurasato/token.txt:/run/secrets/token:ro \
#     ghcr.io/nananek/sakurasato-tui:latest \
#     --api-url https://sakurasato.tailnet.example.ts.net:8443 \
#     --token-file /run/secrets/token
#
#   # docker compose の named volume `local_sock` (= server が `local.sock` を
#   # 置く UDS) をホストから直接マウントする。volume 名は `<project>_<vol>` の
#   # 規約で `sakurasato_local_sock` (= `docker volume ls` で確認可能)。
#   docker run --rm -it \
#     --cap-drop=ALL --security-opt=no-new-privileges:true --read-only \
#     --tmpfs /tmp \
#     -e TERM \
#     -v sakurasato_local_sock:/run/sakurasato:ro \
#     -v $HOME/.config/sakurasato/token.txt:/run/secrets/token:ro \
#     ghcr.io/nananek/sakurasato-tui:latest \
#     --token-file /run/secrets/token
#
# 注意:
#   - TTY 必須 (`-it`)。`TERM` を環境変数で渡す (Kitty graphics には xterm-kitty 等)。
#   - **セキュリティ強化**: docker-compose の `x-hardening` と揃え、`--cap-drop=ALL`
#     + `no-new-privileges` + `--read-only` + `--tmpfs /tmp` を全例で付与する。
#     TUI は純粋な端末 UI なので Linux capability は一切不要。
#   - TCP 接続時は `--network host` で tailnet IP / 公開ドメインに直接到達するのが楽。
#     ただし `--network host` は **Linux Docker 専用** ── macOS / Windows
#     (Docker Desktop) では host network namespace に入れないため、別途
#     `-p` でポート公開 + `--api-url https://host.docker.internal:...` の構成にする。
#   - **TCP は HTTPS を使うこと**。tailnet ACL に守られた経路では `http://` でも
#     秘匿は担保されるが、コピペで `http://` を直 IP / 公開ドメインに使うと
#     Bearer トークンが平文で流れる。例は HTTPS で統一して安全側に倒す。
#   - UDS 接続時は server compose の named volume を `-v sakurasato_local_sock:...`
#     でマウントする。`--socket /run/sakurasato-local/local.sock` を渡す
#     代替経路もあるが、引数追加無しで動く volume 名指定を例示する。

# ---- builder ----
FROM rust:1.97-alpine AS builder

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
# `crates/core/build.rs` が `vendor/gemoji/emoji.json` を読んで Unicode emoji
# テーブルを生成する (PR #122 で導入)。TUI build が core を引くので必須。
COPY vendor ./vendor

# BuildKit のキャッシュマウントで registry とビルド成果物を温存。
# SQLX_OFFLINE=true は workspace 内で sqlx を使う他クレートのコンパイル時クエリ
# 検証用 (TUI 自身は DB を直接叩かないので不要だが、workspace ビルド共通の
# キャッシュを揃えるため server / media-proxy と同じ env を指定する)。
#
# registry cache の `id=` を service ごとに分ける根拠は server.Dockerfile 参照
# (= compose 並列ビルドの race 回避)。
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=sakurasato-tui-registry \
    --mount=type=cache,target=/build/target,id=sakurasato-tui-target \
    SQLX_OFFLINE=true cargo build --release --locked --target x86_64-unknown-linux-musl -p sakurasato-tui && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-tui /sakurasato-tui

# `--read-only` で起動するときの mount target stub。distroless/static には
# `/run/*` 系のディレクトリが存在せず、root fs read-only の状態では OCI
# runtime が掘れないため、build 時に空 dir を作って所有者を nonroot に倒しておく。
# - `/run/sakurasato` … server compose の UDS をマウントする標準位置
# - `/run/secrets` … `--token-file /run/secrets/token` の bind mount 標準位置
# server.Dockerfile と同じパターン (`/stub/run-sakurasato`) を踏襲。
RUN mkdir -p /stub/run-sakurasato /stub/run-secrets

# ---- runtime ----
FROM gcr.io/distroless/static:nonroot AS runtime

WORKDIR /app
COPY --from=builder /sakurasato-tui /app/sakurasato-tui
# `--read-only` 起動向けに mount target を `nonroot:nonroot` 所有で確保する。
# `/run/sakurasato` は 0700 (UDS が個人 secret 扱い)、`/run/secrets` は 0755
# (中の個別ファイルは別途 0400 にする運用) で揃える。
COPY --from=builder --chown=nonroot:nonroot --chmod=0700 \
     /stub/run-sakurasato /run/sakurasato
COPY --from=builder --chown=nonroot:nonroot --chmod=0755 \
     /stub/run-secrets /run/secrets

USER nonroot:nonroot
ENTRYPOINT ["/app/sakurasato-tui"]
# 引数は呼び出し側で必ず指定する (--api-url / --token-file 等)。
CMD []
