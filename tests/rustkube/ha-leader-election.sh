#!/bin/sh
# QA-Name: leader-election lease held
# QA-Owner: glennswest/rustkube
# QA-Desc: a coordination.k8s.io lease in kube-system has a non-empty holderIdentity, so exactly one controller/scheduler leads across the masters
# QA-Scope: cluster
# QA-Topology: full
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"
curl -s "$API/apis/coordination.k8s.io/v1/namespaces/kube-system/leases" | grep -q '"holderIdentity":"[^"]' \
  || { echo "no held leader-election lease in kube-system"; exit 1; }
echo "leader-election lease held"
