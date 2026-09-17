# syntax=docker/dockerfile:1.7
# versity/versitygw を non-root で動かすための薄いラッパ。
# /data を 65534:65534 で作っておけば、新規 named volume に対して
# docker がその所有権ごと初期化してくれる (既存 volume は影響なし)。

FROM versity/versitygw:v1.8.0

USER root
RUN mkdir -p /data && chown -R 65534:65534 /data
USER 65534:65534
