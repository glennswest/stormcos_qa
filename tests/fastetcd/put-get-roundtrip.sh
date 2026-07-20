#!/bin/sh
# QA-Name: etcd v3 put/get round-trip
# QA-Owner: glennswest/fastetcd
# QA-Desc: a put is read back with the same value, then deleted (self-cleaning)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu

EP="${FASTETCD_ENDPOINT:-http://127.0.0.1:2379}"
KEY="/qa/fastetcd/roundtrip"
VAL="qa-roundtrip-value"

# Clear any leftover, and always clean up on exit.
cleanup() { $QA_SSH "fastetcd-ctl --endpoint '$EP' del '$KEY'" >/dev/null 2>&1 || true; }
cleanup
trap cleanup EXIT INT TERM

$QA_SSH "fastetcd-ctl --endpoint '$EP' put '$KEY' '$VAL'" >/dev/null \
  || { echo "put failed"; exit 1; }

got=$($QA_SSH "fastetcd-ctl --endpoint '$EP' get '$KEY'" 2>/dev/null | sed -n '2p')
[ "$got" = "$VAL" ] || { echo "get returned '$got', expected '$VAL'"; exit 1; }
echo "put/get round-trip ok"
