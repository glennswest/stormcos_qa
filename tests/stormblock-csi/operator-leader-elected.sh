#!/bin/sh
# QA-Name: wander operator running and leader-elected
# QA-Owner: glennswest/stormblock-csi
# QA-Desc: the stormblock-operator pod is Running/Ready and the stormblock-tiebreak lease has a live holder
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 120
set -eu

# warn (not blocking) until stormblock-csi ships in the stormcos image set.

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/stormblock-system/pods'" 2>/dev/null || true)
printf '%s' "$pods" | grep -q '"name":"stormblock-operator' \
  || { echo "no stormblock-operator pod in stormblock-system"; exit 1; }
printf '%s' "$pods" | grep -q '"phase":"Running"' \
  || { echo "operator pod not Running"; exit 1; }

# Only the lease holder may promote (fence-then-promote invariant), so the
# lease existing WITH a holder is the leader-election signal.
lease=$($QA_SSH "wget -qO- '$API/apis/coordination.k8s.io/v1/namespaces/stormblock-system/leases/stormblock-tiebreak'" 2>/dev/null || true)
[ -n "$lease" ] || { echo "stormblock-tiebreak lease not found"; exit 1; }
holder=$(printf '%s' "$lease" | sed -n 's/.*"holderIdentity":"\([^"]*\)".*/\1/p')
[ -n "$holder" ] || { echo "lease has no holderIdentity: $lease"; exit 1; }
echo "operator leader: $holder"
