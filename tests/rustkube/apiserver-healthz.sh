#!/bin/sh
# QA-Name: apiserver /healthz ok
# QA-Owner: glennswest/rustkube
# QA-Desc: the rustkube apiserver answers /healthz and serves the node list
# QA-Scope: cluster
# QA-Severity: blocking
set -eu
h=$($QA_SSH "wget -qO- ${QA_API:-http://127.0.0.1:6443}/healthz" 2>/dev/null || true)
[ "$h" = "ok" ] || { echo "healthz != ok: '$h'"; exit 1; }
$QA_SSH "wget -qO- ${QA_API:-http://127.0.0.1:6443}/api/v1/nodes" | grep -q NodeList \
  || { echo "node list not served"; exit 1; }
echo "apiserver healthy"
