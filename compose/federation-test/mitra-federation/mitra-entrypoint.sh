#!/bin/sh
# Mitra entrypoint for federation tests — trusts the shared test CA,
# creates a default `bob` account, and starts the server.
#
# Ed25519 主対向の 1 つ (Mitra は FEP-521a Multikey 対応)。
set -e

if [ -f /certs/ca.crt ]; then
  cp /certs/ca.crt /usr/local/share/ca-certificates/test-federation-ca.crt
  update-ca-certificates 2>/dev/null || true
  echo "Added test CA cert to trust store"
fi

mkdir -p /var/lib/mitra/www

(
  sleep 10
  mitra create-account bob password123 user 2>/dev/null || true
  echo "Created test user bob"
) &

exec mitra server
