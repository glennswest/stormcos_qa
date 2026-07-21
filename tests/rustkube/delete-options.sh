#!/bin/sh
# QA-Name: DeleteOptions preconditions and dryRun honored
# QA-Owner: glennswest/rustkube
# QA-Desc: a resourceVersion precondition mismatch is 409, dryRun does not delete, and a real delete removes the object
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
N="qa-del-$$"
CM="$API/api/v1/namespaces/default/configmaps/$N"
cleanup() { curl -s -X DELETE "$CM" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM

curl -s -o /dev/null -X POST -H 'Content-Type: application/json' \
  -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"'"$N"'"},"data":{"k":"v"}}' \
  "$API/api/v1/namespaces/default/configmaps"

# dryRun delete must NOT remove it
curl -s -o /dev/null -X DELETE -H 'Content-Type: application/json' -d '{"dryRun":["All"]}' "$CM"
code=$(curl -s -o /dev/null -w '%{http_code}' "$CM")
[ "$code" = 200 ] || { echo "dryRun delete removed the object (now $code)"; exit 1; }

# wrong resourceVersion precondition must 409
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H 'Content-Type: application/json' \
  -d '{"preconditions":{"resourceVersion":"1"}}' "$CM")
[ "$code" = 409 ] || { echo "bad-precondition delete returned $code (want 409)"; exit 1; }

# real delete removes it
curl -s -o /dev/null -X DELETE "$CM"
code=$(curl -s -o /dev/null -w '%{http_code}' "$CM")
[ "$code" = 404 ] || { echo "object still present after delete ($code)"; exit 1; }
echo "DeleteOptions: dryRun no-op, precondition 409, delete removes"
REMOTE
