#!/bin/sh
# QA-Name: server-side apply upserts and tracks managedFields
# QA-Owner: glennswest/rustkube
# QA-Desc: apply of a missing object CREATEs it and records metadata.managedFields; a foreign-owned change conflicts unless forced (#45)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"
N="qa-ssa-$$"; CM="$API/api/v1/namespaces/default/configmaps/$N"
cleanup() { curl -s -X DELETE "$CM" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
apply() { curl -s -o /dev/null -w '%{http_code}' -X PATCH -H 'Content-Type: application/apply-patch+yaml' -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$N"'"},"data":'"$3"'}' "$CM?fieldManager=$1&force=$2"; }
[ "$(apply op-a false '{"a":"1"}')" = 200 ] || { echo "apply-create not 200"; exit 1; }
obj=$(curl -s "$CM")
echo "$obj" | grep -q '"managedFields"' || { echo "managedFields missing: $obj"; exit 1; }
echo "$obj" | grep -q '"manager":"op-a"' || { echo "manager op-a missing"; exit 1; }
[ "$(apply op-b false '{"a":"2"}')" = 409 ] || { echo "expected 409 conflict"; exit 1; }
[ "$(apply op-b true '{"a":"2"}')" = 200 ] || { echo "forced apply not 200"; exit 1; }
echo "SSA upsert + managedFields + conflict/force correct"
