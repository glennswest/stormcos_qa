#!/bin/sh
# QA-Name: DaemonSet one pod per node, no churn
# QA-Owner: glennswest/rustkube
# QA-Desc: a DaemonSet schedules exactly one pod per eligible node, status current=desired=node-count, no miscount/churn (#38/#44)
# QA-Scope: cluster
# QA-Topology: multi-node
# QA-Severity: blocking
# QA-Timeout: 240
set -eu
API="http://${QA_NODE_IP}:6443"; N="qa-ds-$$"
cleanup() { curl -s -o /dev/null -X DELETE "$API/apis/apps/v1/namespaces/default/daemonsets/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
exp=$(( $(echo $QA_MASTERS | wc -w) + $(echo $QA_NODES | wc -w) ))
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"apps/v1","kind":"DaemonSet","metadata":{"name":"'"$N"'"},"spec":{"selector":{"matchLabels":{"app":"'"$N"'"}},"template":{"metadata":{"labels":{"app":"'"$N"'"}},"spec":{"tolerations":[{"operator":"Exists"}],"containers":[{"name":"pause","image":"registry.k8s.io/pause:3.9"}]}}}}' "$API/apis/apps/v1/namespaces/default/daemonsets"
cur=0; i=0
while [ $i -lt 40 ]; do
  st=$(curl -s "$API/apis/apps/v1/namespaces/default/daemonsets/$N")
  cur=$(echo "$st" | grep -o '"currentNumberScheduled":[0-9]*' | grep -o '[0-9]*' || echo 0)
  des=$(echo "$st" | grep -o '"desiredNumberScheduled":[0-9]*' | grep -o '[0-9]*' || echo 0)
  [ "${des:-0}" = "$exp" ] && [ "${cur:-0}" = "$exp" ] && break
  i=$((i+1)); sleep 5
done
[ "${cur:-0}" = "$exp" ] || { echo "currentNumberScheduled=${cur:-0}, want $exp (miscount/churn — #44)"; exit 1; }
pods=$(curl -s "$API/api/v1/namespaces/default/pods?labelSelector=app%3D$N" | grep -o '"uid"' | wc -l | tr -d ' ')
[ "$pods" -le "$exp" ] || { echo "$pods DS pods for $exp nodes (duplicates/churn)"; exit 1; }
echo "DaemonSet: one pod per node, current=desired=$exp"
