#!/bin/sh
# QA-Name: fastetcd reports healthy
# QA-Owner: glennswest/fastetcd
# QA-Desc: the etcd-compat GET /health endpoint returns {"health":"true"}
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu

EP="${FASTETCD_ENDPOINT:-http://127.0.0.1:2379}"
h=$($QA_SSH "wget -qO- '$EP/health'" 2>/dev/null || true)
case "$h" in
  *'"health":"true"'*) echo "fastetcd healthy: $h" ;;
  *) echo "fastetcd /health not healthy: '$h'"; exit 1 ;;
esac
