#!/bin/sh
# QA-Name: DeleteOptions preconditions and dryRun honored
# QA-Owner: glennswest/rustkube
# QA-Desc: a resourceVersion precondition mismatch is 409, dryRun does not delete, a real delete removes the object
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"
N="qa-del-$$"; CM="$API/api/v1/namespaces/default/configmaps/$N"
cleanup() { curl -s -X DELETE "$CM" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
curl -s -o /dev/null -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$N"'"},"data":{"k":"v"}}' "$API/api/v1/namespaces/default/configmaps"
curl -s -o /dev/null -X DELETE -H 'Content-Type: application/json' -d '{"dryRun":["All"]}' "$CM"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$CM")" = 200 ] || { echo "dryRun delete removed it"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H 'Content-Type: application/json' -d '{"preconditions":{"resourceVersion":"1"}}' "$CM")" = 409 ] || { echo "bad-precondition not 409"; exit 1; }
curl -s -o /dev/null -X DELETE "$CM"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$CM")" = 404 ] || { echo "still present after delete"; exit 1; }
echo "DeleteOptions: dryRun no-op, precondition 409, delete removes"
