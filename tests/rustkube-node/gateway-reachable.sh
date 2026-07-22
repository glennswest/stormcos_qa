#!/bin/sh
# QA-Name: default gateway reachable
# QA-Owner: glennswest/rustkube-node
# QA-Desc: the node can reach its default gateway (node->gateway path)
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 30
set -eu
# warn, not blocking: on a pre-CNI node the network-operator has not programmed
# full networking yet, so a missing gateway may be expected. Tracked by
# rustkube-node#32 (node<->network-operator coordination). Reports, never tombstones.
gw=$($QA_SSH "ip route show default | awk '{print \$3; exit}'" 2>/dev/null || true)
[ -n "$gw" ] || { echo "no default route configured"; exit 1; }
echo "default gateway: $gw"
$QA_SSH "ping -c1 -W2 $gw >/dev/null 2>&1" \
  || { echo "gateway $gw unreachable (see rustkube-node#32)"; exit 1; }
echo "gateway $gw reachable"
