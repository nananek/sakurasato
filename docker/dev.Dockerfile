# syntax=docker/dockerfile:1.7
# Sakurasato dev toolchain — Docker 内で cargo (fmt/clippy/test/build) を回すための
# 開発専用イメージ。ホストに rust を入れず「ビルドは常に Docker」で完結させる
# ためのもの (§9)。release 用の distroless 出力ではなく、workspace を bind mount
# して cargo を実行するだけの薄いラッパ image。
#
# toolchain は release Dockerfile (server / media-proxy / tui) と同じ
# `rust:1.96-alpine` に固定 ── dev と release で同一 rustc / 同一 musl 標的にし、
# 「手元で通ったのに CI/本番ビルドで落ちる」を避ける。
FROM rust:1.98-alpine

# release builder と同じ最小依存 (musl-dev pkgconfig)。加えて dev 体験用に
# clippy / rustfmt component と git/bash を足す (rust:alpine は既定で clippy /
# rustfmt を含むが、明示 add して将来の base 変更に対して固定する)。
RUN apk add --no-cache musl-dev pkgconfig git bash \
 && rustup component add clippy rustfmt

WORKDIR /build

# sqlx compile-time query 検証は .sqlx キャッシュから引く (DB 不要でビルド可能)。
ENV SQLX_OFFLINE=true \
    CARGO_TERM_COLOR=always
