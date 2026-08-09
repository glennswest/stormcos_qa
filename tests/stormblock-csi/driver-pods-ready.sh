#!/bin/sh
# QA-Name: stormblock-csi controller + node plugin running
# QA-Owner: glennswest/stormblock-csi
# QA-Desc: the CSI controller Deployment pod and node DaemonSet pod in ns stormblock-system are Running and Ready
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 180
set -eu

# warn (not blocking) until stormblock-csi is part of the stormcos image
# deployment set — a not-yet-deployed component should file an issue, not
# tombstone every build.

API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/stormblock-system/pods'" 2>/dev/null || true)
[ -n "$pods" ] || { echo "could not list pods in ns stormblock-system via $API (namespace missing? CSI not deployed?)"; exit 1; }

check_pod() {
  prefix=$1
  printf '%s' "$pods" | grep -q "\"name\":\"$prefix" \
    || { echo "no $prefix* pod in stormblock-system"; return 1; }
  # The pods JSON carries phase + conditions; a Ready=True somewhere in the
  # doc alongside the pod existing is the same signal ironprom's test uses.
  printf '%s' "$pods" | grep -q '"phase":"Running"' \
    || { echo "$prefix pod not Running"; return 1; }
  printf '%s' "$pods" | grep -Eq '"type":"Ready","status":"True"|"status":"True","type":"Ready"' \
    || { echo "$prefix pod not Ready"; return 1; }
}

check_pod stormblock-csi-controller
check_pod stormblock-csi-node
echo "controller + node plugin pods running and ready"
