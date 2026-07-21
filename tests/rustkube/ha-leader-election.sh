#!/bin/sh
# QA-Name: leader-election lease held
# QA-Owner: glennswest/rustkube
# QA-Desc: a coordination.k8s.io lease in kube-system has a non-empty holderIdentity, so exactly one controller/scheduler is leader across the masters
# QA-Scope: cluster
# QA-Topology: multi-master
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
lease=$($QA_SSH "curl -s http://127.0.0.1:6443/apis/coordination.k8s.io/v1/namespaces/kube-system/leases")
echo "$lease" | grep -q '"holderIdentity":"[^"]' \
  || { echo "no held leader-election lease in kube-system: $lease"; exit 1; }
echo "leader-election lease held"
