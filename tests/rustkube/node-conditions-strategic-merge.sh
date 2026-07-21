#!/bin/sh
# QA-Name: node status.conditions strategic-merge preserves Ready
# QA-Owner: glennswest/rustkube
# QA-Desc: a strategic-merge PATCH adding NetworkUnavailable upserts by type and keeps the existing conditions, so a node stays Ready after Cilium (#47)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 90
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
N="qa-cond-$$"
cleanup() { curl -s -X DELETE "$API/api/v1/nodes/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM

curl -s -o /dev/null -X POST -H 'Content-Type: application/json' \
  -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"'"$N"'"}}' "$API/api/v1/nodes"

# kubelet-style: set the four owned conditions
curl -s -o /dev/null -X PUT -H 'Content-Type: application/json' \
  -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"'"$N"'"},"status":{"conditions":[{"type":"Ready","status":"True"},{"type":"MemoryPressure","status":"False"},{"type":"DiskPressure","status":"False"},{"type":"PIDPressure","status":"False"}]}}' \
  "$API/api/v1/nodes/$N/status"

# Cilium-style: strategic-merge just NetworkUnavailable
curl -s -o /dev/null -X PATCH -H 'Content-Type: application/strategic-merge-patch+json' \
  -d '{"status":{"conditions":[{"type":"NetworkUnavailable","status":"False","reason":"QA"}]}}' \
  "$API/api/v1/nodes/$N/status"

got=$(curl -s "$API/api/v1/nodes/$N/status")
for t in Ready MemoryPressure DiskPressure PIDPressure NetworkUnavailable; do
  echo "$got" | grep -q "\"type\":\"$t\"" \
    || { echo "condition $t missing after strategic-merge patch (list was replaced, not merged)"; exit 1; }
done
echo "all 5 conditions present — Ready survived the NetworkUnavailable patch"
REMOTE
