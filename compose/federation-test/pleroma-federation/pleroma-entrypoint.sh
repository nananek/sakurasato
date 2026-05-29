#!/bin/sh
# Pleroma entrypoint for federation tests — trusts the shared test CA so
# Pleroma's outbound TLS to https://sakurasato/ works, then hands off to
# Pleroma's own start script.
if [ -f /certs/ca.crt ]; then
  cp /certs/ca.crt /usr/local/share/ca-certificates/test-federation-ca.crt
  update-ca-certificates 2>/dev/null
  echo "Added test CA cert to trust store"
fi

exec /app/start.sh
