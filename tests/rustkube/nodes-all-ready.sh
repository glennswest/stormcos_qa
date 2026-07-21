#!/bin/sh
# QA-Name: all nodes Ready
# QA-Owner: glennswest/rustkube
# QA-Desc: every master-node and worker reports Ready=True (kubelet registered + CNI up)
# QA-Scope: cluster
# QA-Topology: full
# QA-Severity: blocking
# QA-Timeout: 120
set -eu
exp=$(( $(echo $QA_MASTERS | wc -w) + $(echo $QA_NODES | wc -w) ))
nodes=$($QA_SSH "curl -s http://127.0.0.1:6443/api/v1/nodes")
notready=$(echo "$nodes" | grep -o '"type":"Ready","status":"False"' | wc -l | tr -d ' ')
ready=$(echo "$nodes" | grep -o '"type":"Ready","status":"True"' | wc -l | tr -d ' ')
[ "$notready" -eq 0 ] || { echo "$notready node(s) NotReady"; exit 1; }
[ "$ready" -ge "$exp" ] || { echo "only $ready/$exp nodes Ready"; exit 1; }
echo "all $ready nodes Ready (expected >= $exp)"
