#!/bin/sh
# QA-Name: ironprom serves /-/healthy and /-/ready
# QA-Owner: glennswest/ironprom
# QA-Desc: the ironprom HTTP server answers its liveness/readiness probes
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 120
set -eu

# Resolve ironprom's endpoint: the pod IP on :9090 (routable on the node via
# the CNI), falling back to a host-local :9090.
API="${QA_API:-http://127.0.0.1:6443}"
pods=$($QA_SSH "wget -qO- '$API/api/v1/namespaces/monitoring/pods'" 2>/dev/null || true)
ip=$(printf '%s' "$pods" | grep -oE '"podIP":"[0-9.]+"' | head -1 | grep -oE '[0-9.]+' || true)
EP="http://${ip:-127.0.0.1}:9090"
echo "ironprom endpoint: $EP"

h=$($QA_SSH "wget -qO- '$EP/-/healthy'" 2>/dev/null || true)
case "$h" in
  *Healthy*) echo "healthy: $h" ;;
  *) echo "/-/healthy not healthy: '$h'"; exit 1 ;;
esac

r=$($QA_SSH "wget -qO- '$EP/-/ready'" 2>/dev/null || true)
case "$r" in
  *Ready*) echo "ready: $r" ;;
  *) echo "/-/ready not ready: '$r'"; exit 1 ;;
esac
