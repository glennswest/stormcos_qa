#!/bin/sh
# QA-Name: WatchList emits the initial-events-end bookmark
# QA-Owner: glennswest/rustkube
# QA-Desc: a watch with sendInitialEvents=true streams the current objects then a BOOKMARK annotated k8s.io/initial-events-end=true — the signal client-go informers sync on (#39)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
# namespaces always exist (default/kube-system/...), so the initial stream is non-empty.
out=$(curl -s --max-time 8 \
  "$API/api/v1/namespaces?watch=true&sendInitialEvents=true&allowWatchBookmarks=true&resourceVersionMatch=NotOlderThan" \
  2>/dev/null || true)
echo "$out" | grep -q '"type":"ADDED"' \
  || { echo "no initial ADDED events in the WatchList stream"; exit 1; }
echo "$out" | grep -q '"k8s.io/initial-events-end":"true"' \
  || { echo "no initial-events-end BOOKMARK in the stream"; exit 1; }
echo "WatchList emitted the initial-events-end bookmark"
REMOTE
