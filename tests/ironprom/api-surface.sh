#!/bin/sh
# QA-Name: ironprom /api/v1 surface responds
# QA-Owner: glennswest/ironprom
# QA-Desc: buildinfo, status/tsdb, targets, and labels return the Prometheus JSON envelope
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 120
set -eu

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/monitoring/pods'" 2>/dev/null || true)
ip=$(printf '%s' "$pods" | grep -oE '"podIP":"[0-9.]+"' | head -1 | grep -oE '[0-9.]+' || true)
EP="http://${ip:-127.0.0.1}:9090"

check() { # <path> <must-contain>
  body=$($QA_SSH "wget -qO- '$EP$1'" 2>/dev/null || true)
  printf '%s' "$body" | grep -q '"status":"success"' \
    || { echo "$1 not success: $body"; exit 1; }
  printf '%s' "$body" | grep -q "$2" \
    || { echo "$1 missing '$2': $body"; exit 1; }
  echo "ok $1"
}

# buildinfo carries a version (Grafana's Prometheus detection relies on this).
check "/api/v1/status/buildinfo" '"version"'
# tsdb status reports the head stats structure.
check "/api/v1/status/tsdb" '"headStats"'
# targets endpoint (scrape manager wired up).
check "/api/v1/targets" '"activeTargets"'
# label names endpoint.
check "/api/v1/labels" '"data"'

echo "/api/v1 surface ok"
