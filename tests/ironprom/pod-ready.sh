#!/bin/sh
# QA-Name: ironprom pod is running and ready
# QA-Owner: glennswest/ironprom
# QA-Desc: the ironprom StatefulSet pod in ns monitoring is Running and Ready
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 120
set -eu

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/monitoring/pods'" 2>/dev/null || true)
[ -n "$pods" ] || { echo "could not list pods in ns monitoring via $API"; exit 1; }

# ironprom is the ironprom-0 StatefulSet pod. Confirm it exists, phase Running,
# and the Ready condition is True.
printf '%s' "$pods" | grep -q '"name":"ironprom-0"' \
  || { echo "no ironprom-0 pod found in ns monitoring"; echo "$pods" | head -c 400; exit 1; }
printf '%s' "$pods" | grep -q '"phase":"Running"' \
  || { echo "ironprom-0 not Running"; exit 1; }
# A Ready condition with status True (the pods JSON carries conditions).
printf '%s' "$pods" | grep -Eq '"type":"Ready","status":"True"|"status":"True","type":"Ready"' \
  || { echo "ironprom-0 not Ready"; exit 1; }

echo "ironprom-0 running and ready"
