#!/bin/sh
# QA-Name: JSON-Patch test-null against an absent path holds
# QA-Owner: glennswest/rustkube
# QA-Desc: cilium-operator's node-taint CAS [{test /spec/taints null},{add …}] succeeds (evanphx leniency), not "path is invalid"
# QA-Scope: cluster
# QA-Topology: single
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"; N="qa-jp-$$"
cleanup() { curl -s -X DELETE "$API/api/v1/nodes/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"'"$N"'"}}' "$API/api/v1/nodes"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H 'Content-Type: application/json-patch+json' -d '[{"op":"test","path":"/spec/taints","value":null},{"op":"add","path":"/spec/taints","value":[{"key":"node.cilium.io/agent-not-ready","effect":"NoSchedule"}]}]' "$API/api/v1/nodes/$N")
[ "$code" = 200 ] || { echo "taint CAS got $code (want 200)"; exit 1; }
curl -s "$API/api/v1/nodes/$N" | grep -q 'agent-not-ready' || { echo "taint not applied"; exit 1; }
echo "json-patch test-null CAS applied the taint"
