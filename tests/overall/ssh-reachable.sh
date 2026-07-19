#!/bin/sh
# QA-Name: node is reachable over ssh
# QA-Owner: glennswest/stormcos
# QA-Desc: the provisioned node answers ssh and returns its hostname
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 120
set -eu
hn=$($QA_SSH "hostname" 2>/dev/null || true)
[ -n "$hn" ] || { echo "no ssh response from $QA_NODE_IP"; exit 1; }
echo "ssh ok: $hn"
