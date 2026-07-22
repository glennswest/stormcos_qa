#!/bin/sh
# QA-Name: node has a global IPv4
# QA-Owner: glennswest/rustkube-node
# QA-Desc: the node has a non-loopback, global-scope IPv4 address on an interface
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 30
set -eu
addrs=$($QA_SSH "ip -4 -o addr show scope global" 2>/dev/null || true)
echo "$addrs"
echo "$addrs" | grep -q 'inet ' \
  || { echo "no global IPv4 address on any interface"; exit 1; }
echo "node has a global IPv4"
