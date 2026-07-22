#!/bin/sh
# QA-Name: events.k8s.io/v1 API group served
# QA-Owner: glennswest/rustkube
# QA-Desc: discovery advertises events.k8s.io and /apis/events.k8s.io/v1 lists the events resource (#48)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
# curl runs in the QA process (builder env), hitting the node apiserver directly.
set -eu
API="http://${QA_NODE_IP}:6443"
curl -s "$API/apis" | grep -q '"name":"events.k8s.io"' || { echo "events.k8s.io not advertised"; exit 1; }
res=$(curl -s "$API/apis/events.k8s.io/v1")
echo "$res" | grep -q '"groupVersion":"events.k8s.io/v1"' || { echo "bad group list: $res"; exit 1; }
echo "$res" | grep -q '"kind":"Event"' || { echo "events resource missing: $res"; exit 1; }
echo "events.k8s.io/v1 served"
