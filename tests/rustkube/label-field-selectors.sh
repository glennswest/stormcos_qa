#!/bin/sh
# QA-Name: label and field selectors filter lists
# QA-Owner: glennswest/rustkube
# QA-Desc: labelSelector and fieldSelector narrow a LIST to matching objects
# QA-Scope: cluster
# QA-Topology: single
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"; P="qa-sel-$$"
cleanup() { for n in ${P}-a ${P}-b; do curl -s -X DELETE "$API/api/v1/namespaces/default/configmaps/$n" >/dev/null 2>&1 || true; done; }
cleanup; trap cleanup EXIT INT TERM
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"${P}-a"'","labels":{"qa":"yes"}}}' "$API/api/v1/namespaces/default/configmaps"
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"${P}-b"'","labels":{"qa":"no"}}}' "$API/api/v1/namespaces/default/configmaps"
sel=$(curl -s "$API/api/v1/namespaces/default/configmaps?labelSelector=qa%3Dyes")
echo "$sel" | grep -q "${P}-a" || { echo "labelSelector missed a"; exit 1; }
echo "$sel" | grep -q "${P}-b" && { echo "labelSelector leaked b"; exit 1; }
fs=$(curl -s "$API/api/v1/namespaces/default/configmaps?fieldSelector=metadata.name%3D${P}-a")
echo "$fs" | grep -q "${P}-a" || { echo "fieldSelector missed a"; exit 1; }
echo "$fs" | grep -q "${P}-b" && { echo "fieldSelector leaked b"; exit 1; }
echo "label + field selectors filter correctly"
