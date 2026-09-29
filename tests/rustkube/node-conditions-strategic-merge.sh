#!/bin/sh
# QA-Name: node status.conditions strategic-merge preserves Ready
# QA-Owner: glennswest/rustkube
# QA-Desc: a strategic-merge PATCH adding NetworkUnavailable upserts by type and keeps the existing conditions, so a node stays Ready after Cilium (#47)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 90
set -eu
API="http://${QA_NODE_IP}:6443"
N="qa-cond-$$"
cleanup() { curl -s -X DELETE "$API/api/v1/nodes/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"'"$N"'"}}' "$API/api/v1/nodes"
curl -s -o /dev/null -X PUT -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"'"$N"'"},"status":{"conditions":[{"type":"Ready","status":"True"},{"type":"MemoryPressure","status":"False"},{"type":"DiskPressure","status":"False"},{"type":"PIDPressure","status":"False"}]}}' "$API/api/v1/nodes/$N/status"
curl -s -o /dev/null -X PATCH -H 'Content-Type: application/strategic-merge-patch+json' -d '{"status":{"conditions":[{"type":"NetworkUnavailable","status":"False","reason":"QA"}]}}' "$API/api/v1/nodes/$N/status"
got=$(curl -s "$API/api/v1/nodes/$N/status")
for t in Ready MemoryPressure DiskPressure PIDPressure NetworkUnavailable; do
  echo "$got" | grep -q "\"type\":\"$t\"" || { echo "condition $t dropped (list replaced not merged)"; exit 1; }
done
echo "all 5 conditions present — Ready survived the NetworkUnavailable patch"
