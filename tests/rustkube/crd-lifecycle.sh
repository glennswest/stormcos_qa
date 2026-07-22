#!/bin/sh
# QA-Name: CRD create/establish + CR list uses the real listKind
# QA-Owner: glennswest/rustkube
# QA-Desc: a created CRD gets Established, a CR can be created, and the list is kind=<Kind>List not {plural}List so typed informers decode (#34/#36)
# QA-Scope: cluster
# QA-Topology: single
# QA-Severity: blocking
# QA-Timeout: 90
set -eu
API="http://${QA_NODE_IP}:6443"; G=qa.example.com; CRD="qawidgets.$G"
cleanup() { curl -s -X DELETE "$API/apis/apiextensions.k8s.io/v1/customresourcedefinitions/$CRD" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"'"$CRD"'"},"spec":{"group":"'"$G"'","scope":"Namespaced","names":{"plural":"qawidgets","singular":"qawidget","kind":"QaWidget","listKind":"QaWidgetList"},"versions":[{"name":"v1","served":true,"storage":true,"schema":{"openAPIV3Schema":{"type":"object","x-kubernetes-preserve-unknown-fields":true}}}]}}' "$API/apis/apiextensions.k8s.io/v1/customresourcedefinitions"
i=0; while [ $i -lt 10 ]; do curl -s "$API/apis/$G/v1/namespaces/default/qawidgets" | grep -q QaWidgetList && break; i=$((i+1)); sleep 1; done
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"'"$G"'/v1","kind":"QaWidget","metadata":{"name":"w1"},"spec":{"size":1}}' "$API/apis/$G/v1/namespaces/default/qawidgets"
lk=$(curl -s "$API/apis/$G/v1/namespaces/default/qawidgets")
echo "$lk" | grep -q '"kind":"QaWidgetList"' || { echo "list kind wrong: $lk"; exit 1; }
echo "$lk" | grep -q '"name":"w1"' || { echo "CR not listed"; exit 1; }
echo "CRD established, CR created, listKind=QaWidgetList"
