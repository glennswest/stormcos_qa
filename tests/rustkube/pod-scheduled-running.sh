#!/bin/sh
# QA-Name: pod scheduled and Running
# QA-Owner: glennswest/rustkube
# QA-Desc: a Deployment's pod is scheduled onto a node and reaches phase Running (scheduler + kubelet + CRI + CNI end-to-end)
# QA-Scope: cluster
# QA-Topology: full
# QA-Severity: blocking
# QA-Timeout: 300
set -eu
API=http://127.0.0.1:6443
N="qa-pod-$$"
cleanup() { $QA_SSH "curl -s -o /dev/null -X DELETE $API/apis/apps/v1/namespaces/default/deployments/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
$QA_SSH "curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{\"apiVersion\":\"apps/v1\",\"kind\":\"Deployment\",\"metadata\":{\"name\":\"$N\"},\"spec\":{\"replicas\":1,\"selector\":{\"matchLabels\":{\"app\":\"$N\"}},\"template\":{\"metadata\":{\"labels\":{\"app\":\"$N\"}},\"spec\":{\"containers\":[{\"name\":\"pause\",\"image\":\"registry.k8s.io/pause:3.9\"}]}}}}' $API/apis/apps/v1/namespaces/default/deployments"
i=0; while [ $i -lt 55 ]; do
  pods=$($QA_SSH "curl -s '$API/api/v1/namespaces/default/pods?labelSelector=app%3D$N'")
  echo "$pods" | grep -q '"phase":"Running"' && { echo "pod scheduled and Running"; exit 0; }
  i=$((i+1)); sleep 5
done
echo "no pod reached Running in time:"; echo "$pods" | head -c 600; exit 1
