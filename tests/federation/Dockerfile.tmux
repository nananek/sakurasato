# syntax=docker/dockerfile:1.7
# Federation pytest runner with tmux + sakurasato-tui binary embedded.
#
# `tests/federation/Dockerfile` (= mastodon pytest container) は
# python:3.13-slim + httpx だけの薄いランナで、AP プロトコル経路を直接
# 叩く形だった。本 Dockerfile は #58 (M12) PR2a 以降で必要になる
# **TUI 駆動シナリオ** 用に、tmux + sakurasato-tui 本物バイナリを同梱する。
#
# Build context は **リポジトリルート** (compose 側で `context: ../..` を
# 設定) — `crates/` / `vendor/` / `scripts/tmux-e2e/` 等を COPY する必要が
# あるため。`tests/federation/Dockerfile` が context `tests/federation` で
# 動く設計と意図的に分けてある (= 純 httpx ランナを軽く保つ)。

# ── Stage 1: TUI binary builder ──────────────────────────────
# rust:1.96-alpine + musl で静的 binary を作る。`docker/server.Dockerfile`
# の builder と同じパターン (cache mount + SQLX_OFFLINE)。BuildKit の
# `--mount=type=cache` は CI でも有効化される (= compose build 経由)。
FROM rust:1.96-alpine AS tui-builder

RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /build

# Cargo workspace に必要な全 source を取り込む。`unicode_emoji` の
# `build.rs` は `vendor/gemoji/emoji.json` を読むので vendor も必須。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY config ./config
COPY migrations ./migrations
COPY .sqlx ./.sqlx
COPY vendor ./vendor

# `-p sakurasato-tui` だけ build。server / media-proxy は別 image。
#
# `id=sakurasato-tui-registry` を `docker/tui.Dockerfile` と揃える ── 両者は
# 同 compose stack で並列ビルドされない (= tui.Dockerfile は本番 compose 用 /
# 本 Dockerfile は federation test pytest 用)。federation stack 側では本
# Dockerfile.tmux が server.Dockerfile + media-proxy.Dockerfile と並列に走るが、
# それぞれが別 `id=` を持つので registry mount は衝突しない。
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=sakurasato-tui-registry \
    --mount=type=cache,target=/build/target,id=sakurasato-tui-target \
    SQLX_OFFLINE=true cargo build --release \
        --target x86_64-unknown-linux-musl \
        -p sakurasato-tui && \
    cp target/x86_64-unknown-linux-musl/release/sakurasato-tui /sakurasato-tui

# ── Stage 2: pytest runner ───────────────────────────────────
# python:3.13-slim + tmux + ca-certificates。`tests/federation/Dockerfile`
# と同じ uid (65532) を踏襲して `sakurasato_local_api` 共有 volume を
# 触れるようにする。
FROM python:3.13-slim AS runner

# tmux: pty 駆動の本体。ca-certificates: 共有テスト CA を入れた後に
# `update-ca-certificates` で `/etc/ssl/certs` を更新するための土台。
# procps: `ps` (デバッグで使う) — 入れなくても本体動作には影響しない。
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        tmux ca-certificates procps && \
    rm -rf /var/lib/apt/lists/*

# uid 65532 (= distroless `nonroot`) と揃える。`tests/federation/Dockerfile`
# と同じ理由 (sakurasato-server が `/home/nonroot/local.sock` を 0o600 +
# 65532 所有で bind するため、同 uid でないと open できない)。
RUN groupadd -g 65532 nonroot 2>/dev/null || true && \
    useradd -u 65532 -g 65532 -m -d /home/nonroot -s /usr/sbin/nologin nonroot 2>/dev/null || true

WORKDIR /tests

# pytest 依存関係 (httpx, pytest, pytest-rerunfailures など) を先に入れる。
COPY tests/federation/requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# pytest 本体 (conftest.py, test_*.py)。
COPY tests/federation/ ./

# tmux harness (lib.sh + 補助 TmuxSession). `scripts/tmux-e2e/conftest.py`
# は driver 単体テスト用の fixture も持つので、tests/federation 側の
# conftest はこちらの `TmuxSession` クラス + `_run_lib` ヘルパだけを
# import する想定。
COPY scripts/tmux-e2e/lib.sh /tests/tmux_lib.sh
COPY scripts/tmux-e2e/conftest.py /tests/tmux_driver.py

# TUI バイナリを builder stage から取り込む。`--no-images` で起動するので
# Kitty / Sixel 検出のための libsixel 等は不要。
COPY --from=tui-builder /sakurasato-tui /usr/local/bin/sakurasato-tui

# /tests の所有を 65532 に倒す ── pytest が `.pytest_cache` を cwd に
# 作るため書き込み権が要る。
RUN chown -R 65532:65532 /tests

# Python 側からは `TMUX_E2E_LIB_PATH` で lib.sh の位置を指定できるよう
# にしておく ── 本コンテナでは /tests/tmux_lib.sh にコピー済み。
# `SAKURASATO_TUI_BIN` も同様に PATH 上の binary を指す。
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    TMUX_E2E_LIB_PATH=/tests/tmux_lib.sh \
    SAKURASATO_TUI_BIN=/usr/local/bin/sakurasato-tui

USER 65532:65532

CMD ["pytest", "-xvs", "--tb=short", "test_nekonoverse_tui.py"]
