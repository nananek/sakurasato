#!/bin/sh
# Generate a self-signed CA and per-host server certs for federation tests.
#
# Driven by env var `CERT_DOMAINS` (comma-separated hostnames). Each domain
# gets a cert at /certs/<domain>.{crt,key} signed by /certs/ca.crt. The CA
# itself is generated once and reused across reruns.
#
# Validity is intentionally 1 day — these certs only live as long as the
# compose stack and must never escape the test network.
set -eu

CERT_DIR=/certs
mkdir -p "$CERT_DIR"

if [ -z "${CERT_DOMAINS:-}" ]; then
  echo "CERT_DOMAINS must be set (comma-separated hostnames)" >&2
  exit 1
fi

if [ ! -f "$CERT_DIR/ca.crt" ]; then
  # `keyUsage` / `basicConstraints` を **明示的** に立てる ──
  # newer Python ssl (cpython 3.13+) は CA 証明書に `keyUsage` 拡張が
  # 無いと `CA cert does not include key usage extension` で拒否する。
  # `-x509` だけでは拡張が乗らないので addext で補う (openssl 3.x の作法)。
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$CERT_DIR/ca.key" \
    -out "$CERT_DIR/ca.crt" \
    -days 1 \
    -subj "/CN=Sakurasato Federation Test CA" \
    -addext "basicConstraints = critical, CA:TRUE" \
    -addext "keyUsage = critical, keyCertSign, cRLSign" \
    2>/dev/null
  echo "Generated CA cert"
fi

IFS=','
for domain in $CERT_DOMAINS; do
  if [ -f "$CERT_DIR/$domain.crt" ]; then
    continue
  fi
  openssl req -newkey rsa:2048 -nodes \
    -keyout "$CERT_DIR/$domain.key" \
    -out "$CERT_DIR/$domain.csr" \
    -subj "/CN=$domain" \
    -addext "subjectAltName=DNS:$domain" \
    2>/dev/null
  openssl x509 -req \
    -in "$CERT_DIR/$domain.csr" \
    -CA "$CERT_DIR/ca.crt" \
    -CAkey "$CERT_DIR/ca.key" \
    -CAcreateserial \
    -out "$CERT_DIR/$domain.crt" \
    -days 1 \
    -copy_extensions copyall \
    2>/dev/null
  rm -f "$CERT_DIR/$domain.csr"
  echo "Generated cert for $domain"
done
