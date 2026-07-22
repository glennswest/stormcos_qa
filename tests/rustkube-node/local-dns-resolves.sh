#!/bin/sh
# QA-Name: local DNS resolves
# QA-Owner: glennswest/rustkube-node
# QA-Desc: the node resolves a name through its configured resolver (node->local resolver path)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 30
set -eu
# QA_DNS_PROBE lets a caller override the probe name; default is a stable public name.
probe="${QA_DNS_PROBE:-one.one.one.one}"
$QA_SSH "grep -q '^nameserver' /etc/resolv.conf" \
  || { echo "no nameserver in /etc/resolv.conf"; exit 1; }
out=$($QA_SSH "getent ahostsv4 $probe" 2>/dev/null || true)
echo "$out"
echo "$out" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' \
  || { echo "resolver did not answer for $probe (DNS path down)"; exit 1; }
echo "local DNS resolves ($probe)"
