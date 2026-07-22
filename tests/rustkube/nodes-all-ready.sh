#!/bin/sh
# QA-Name: all nodes Ready
# QA-Owner: glennswest/rustkube
# QA-Desc: every node in the cluster reports Ready=True (kubelet registered + CNI up)
# QA-Scope: cluster
# QA-Topology: multi-node
# QA-Severity: blocking
# QA-Timeout: 120
set -eu
API="http://${QA_NODE_IP}:6443"
exp=$(( $(echo $QA_MASTERS | wc -w) + $(echo $QA_NODES | wc -w) ))
nodes=$(curl -s "$API/api/v1/nodes")
notready=$(echo "$nodes" | grep -o '"type":"Ready","status":"False"' | wc -l | tr -d ' ')
ready=$(echo "$nodes" | grep -o '"type":"Ready","status":"True"' | wc -l | tr -d ' ')
[ "$notready" -eq 0 ] || { echo "$notready node(s) NotReady"; exit 1; }
[ "$ready" -ge "$exp" ] || { echo "only $ready/$exp nodes Ready"; exit 1; }
echo "all $ready nodes Ready (>= $exp)"
