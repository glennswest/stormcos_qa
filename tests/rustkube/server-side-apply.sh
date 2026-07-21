#!/bin/sh
# QA-Name: server-side apply upserts and tracks managedFields
# QA-Owner: glennswest/rustkube
# QA-Desc: apply of a missing object CREATEs it and records metadata.managedFields; a foreign-owned change conflicts unless forced (#45)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
N="qa-ssa-$$"
CM="$API/api/v1/namespaces/default/configmaps/$N"
cleanup() { curl -s -X DELETE "$CM" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM

apply() { # manager, force, data
  curl -s -o /dev/null -w '%{http_code}' -X PATCH \
    -H 'Content-Type: application/apply-patch+yaml' \
    -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$N"'"},"data":'"$3"'}' \
    "$CM?fieldManager=$1&force=$2"
}

# apply a NEW object -> must CREATE (200), not 404
code=$(apply op-a false '{"a":"1"}')
[ "$code" = 200 ] || { echo "apply-create returned $code (want 200)"; exit 1; }

obj=$(curl -s "$CM")
echo "$obj" | grep -q '"managedFields"' || { echo "managedFields not recorded: $obj"; exit 1; }
echo "$obj" | grep -q '"manager":"op-a"' || { echo "field manager op-a not recorded: $obj"; exit 1; }

# a second manager changing op-a's field must conflict (409) without force
code=$(apply op-b false '{"a":"2"}')
[ "$code" = 409 ] || { echo "expected 409 conflict, got $code"; exit 1; }

# and succeed with force
code=$(apply op-b true '{"a":"2"}')
[ "$code" = 200 ] || { echo "forced apply returned $code (want 200)"; exit 1; }
echo "SSA upsert + managedFields + conflict/force all correct"
REMOTE
