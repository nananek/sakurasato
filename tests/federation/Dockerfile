# syntax=docker/dockerfile:1.7
# pytest runner for federation tests.
#
# Runs as uid 65532 (distroless `nonroot`) so it can read the sakurasato
# server's UDS at /home/nonroot/local.sock (mode 0o600, owned by 65532).
# The named volume `sakurasato_local_api` is mounted at /home/nonroot in
# the compose file; the volume initialises with that ownership because
# distroless/static seeds /home/nonroot as 0o700 65532:65532.
FROM python:3.13-slim

# uid を distroless/static の nonroot (65532) と揃える。
# システムに既に居る場合 (= base image が用意してくれている場合) は
# `useradd` が失敗するので || true で受ける。
RUN groupadd -g 65532 nonroot 2>/dev/null || true && \
    useradd -u 65532 -g 65532 -m -d /home/nonroot -s /usr/sbin/nologin nonroot 2>/dev/null || true

WORKDIR /tests

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY . ./
# /tests を 65532:65532 で持たせる。pytest が `.pytest_cache` を
# (PYTHONDONTWRITEBYTECODE=1 でも) cwd に作ろうとするため、書き込み権が
# 必要。`-p no:cacheprovider` で済ますことも出来るが、テスト追加時の
# 副作用を読みづらくするのでディレクトリ自体を書ける状態にしておく。
RUN chown -R 65532:65532 /tests

# pytest 実行時の cwd は /tests。`/home/nonroot` は compose 側で named
# volume にマウントされるので、ここでの所有権はランタイムでは上書きされる。
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1
USER 65532:65532

CMD ["pytest", "-xvs", "--tb=short", "test_mastodon.py"]
