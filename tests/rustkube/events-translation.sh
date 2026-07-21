#!/bin/sh
# QA-Name: Event translates between events.k8s.io/v1 and core/v1
# QA-Owner: glennswest/rustkube
# QA-Desc: an Event created via events.k8s.io/v1 (regarding/note) reads back as core/v1 (involvedObject/message) — same stored object (#48)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
N="qa-ev-$$"
cleanup() { curl -s -X DELETE "$API/apis/events.k8s.io/v1/namespaces/default/events/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM

body='{"apiVersion":"events.k8s.io/v1","kind":"Event","metadata":{"name":"'"$N"'"},"regarding":{"kind":"Pod","name":"qa-pod"},"note":"qa note","reason":"QA","type":"Normal","reportingController":"qa","eventTime":"2026-01-01T00:00:00.000000Z"}'
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d "$body" "$API/apis/events.k8s.io/v1/namespaces/default/events")
[ "$code" = 201 ] || { echo "create via events.k8s.io/v1 got $code"; exit 1; }

# read back as events.k8s.io/v1
v1=$(curl -s "$API/apis/events.k8s.io/v1/namespaces/default/events/$N")
echo "$v1" | grep -q '"note":"qa note"' || { echo "events.k8s.io/v1 note missing: $v1"; exit 1; }
echo "$v1" | grep -q '"regarding"' || { echo "events.k8s.io/v1 regarding missing: $v1"; exit 1; }

# read the SAME object as core/v1 — must be translated
core=$(curl -s "$API/api/v1/namespaces/default/events/$N")
echo "$core" | grep -q '"message":"qa note"' || { echo "core/v1 message not translated: $core"; exit 1; }
echo "$core" | grep -q '"involvedObject"' || { echo "core/v1 involvedObject not translated: $core"; exit 1; }
echo "Event round-trips both API representations"
REMOTE
