#!/bin/sh
# QA-Name: node reports Ready
# QA-Owner: glennswest/rustkube-node
# QA-Desc: kubelet registered the node and it reports Ready=True
# QA-Scope: cluster
# QA-Severity: blocking
set -eu
nodes=$($QA_SSH "wget -qO- ${QA_API:-http://127.0.0.1:6443}/api/v1/nodes" 2>/dev/null || true)
echo "$nodes" | grep -q '"type":"Ready","status":"True"' \
  || { echo "node not Ready: $nodes"; exit 1; }
echo "node Ready"
