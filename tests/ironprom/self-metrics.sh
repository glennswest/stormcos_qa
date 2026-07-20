#!/bin/sh
# QA-Name: ironprom exposes its self-metrics
# QA-Owner: glennswest/ironprom
# QA-Desc: /metrics exposes the ironprom_* gauges (head series, WAL, blocks)
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 120
set -eu

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/monitoring/pods'" 2>/dev/null || true)
ip=$(printf '%s' "$pods" | grep -oE '"podIP":"[0-9.]+"' | head -1 | grep -oE '[0-9.]+' || true)
EP="http://${ip:-127.0.0.1}:9090"

m=$($QA_SSH "wget -qO- '$EP/metrics'" 2>/dev/null || true)
[ -n "$m" ] || { echo "/metrics empty"; exit 1; }

for want in ironprom_head_series ironprom_head_samples_appended_total ironprom_blocks; do
  printf '%s' "$m" | grep -q "^$want " \
    || { echo "/metrics missing $want"; exit 1; }
done
echo "self-metrics present"
