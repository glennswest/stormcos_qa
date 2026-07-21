#!/bin/sh
# QA-Name: events.k8s.io/v1 API group served
# QA-Owner: glennswest/rustkube
# QA-Desc: discovery advertises events.k8s.io and /apis/events.k8s.io/v1 lists the events resource (#48)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
#
# The QA node serves the apiserver on plaintext HTTP with anonymous access
# (dev mode), like tests/rustkube-node/node-ready.sh.
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
groups=$(curl -s "$API/apis" || true)
echo "$groups" | grep -q '"name":"events.k8s.io"' \
  || { echo "events.k8s.io not advertised in /apis"; exit 1; }
res=$(curl -s "$API/apis/events.k8s.io/v1" || true)
echo "$res" | grep -q '"groupVersion":"events.k8s.io/v1"' \
  || { echo "bad /apis/events.k8s.io/v1: $res"; exit 1; }
echo "$res" | grep -q '"kind":"Event"' \
  || { echo "events resource not listed: $res"; exit 1; }
echo "events.k8s.io/v1 served"
REMOTE
