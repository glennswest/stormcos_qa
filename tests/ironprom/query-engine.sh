#!/bin/sh
# QA-Name: ironprom PromQL engine evaluates queries
# QA-Owner: glennswest/ironprom
# QA-Desc: /api/v1/query answers scalar math and a function in the real image
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 120
set -eu

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/monitoring/pods'" 2>/dev/null || true)
ip=$(printf '%s' "$pods" | grep -oE '"podIP":"[0-9.]+"' | head -1 | grep -oE '[0-9.]+' || true)
EP="http://${ip:-127.0.0.1}:9090"

# Scalar arithmetic: 1+1 -> 2. Proves the engine runs end to end (no data
# needed), independent of what the cluster happens to be scraping.
r=$($QA_SSH "wget -qO- '$EP/api/v1/query?query=1%2B1'" 2>/dev/null || true)
printf '%s' "$r" | grep -q '"status":"success"' \
  || { echo "query 1+1 not success: $r"; exit 1; }
printf '%s' "$r" | grep -q '"result":\["' 2>/dev/null || true
echo "$r" | grep -q '"2"' \
  || { echo "1+1 did not evaluate to 2: $r"; exit 1; }

# A function evaluated over an instant vector: clamp_max(vector(9),5) -> 5.
r=$($QA_SSH "wget -qO- '$EP/api/v1/query?query=clamp_max(vector(9),5)'" 2>/dev/null || true)
printf '%s' "$r" | grep -q '"status":"success"' \
  || { echo "clamp_max query failed: $r"; exit 1; }
echo "$r" | grep -q '"5"' \
  || { echo "clamp_max(vector(9),5) != 5: $r"; exit 1; }

echo "PromQL engine ok (1+1=2, clamp_max=5)"
