#!/bin/sh
# QA-Name: outbound network reachable
# QA-Owner: glennswest/rustkube-node
# QA-Desc: the node can reach the internet directly (node->gateway->internet path)
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 30
set -eu
# warn, not blocking: same rationale as gateway-reachable — outbound may depend on
# the network-operator (CNI) being up. Tracked by rustkube-node#32. A zeroboot
# cluster with preloaded images + local DNS does not require this to function.
probe="${QA_OUTBOUND_PROBE:-8.8.8.8}"
port="${QA_OUTBOUND_PORT:-53}"
# Prefer a TCP connect (no CAP_NET_RAW / ping binary needed); fall back to ping.
if $QA_SSH "command -v bash >/dev/null 2>&1"; then
  $QA_SSH "timeout 3 bash -c 'echo > /dev/tcp/$probe/$port' 2>/dev/null" \
    && { echo "outbound OK (tcp $probe:$port)"; exit 0; }
fi
$QA_SSH "ping -c1 -W2 $probe >/dev/null 2>&1" \
  && { echo "outbound OK (ping $probe)"; exit 0; }
echo "outbound unreachable ($probe) — see rustkube-node#32"
exit 1
