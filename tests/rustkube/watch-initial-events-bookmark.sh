#!/bin/sh
# QA-Name: WatchList emits the initial-events-end bookmark
# QA-Owner: glennswest/rustkube
# QA-Desc: a watch with sendInitialEvents=true streams current objects then a BOOKMARK annotated k8s.io/initial-events-end=true (#39)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"
out=$(curl -s --max-time 8 "$API/api/v1/namespaces?watch=true&sendInitialEvents=true&allowWatchBookmarks=true&resourceVersionMatch=NotOlderThan" 2>/dev/null || true)
echo "$out" | grep -q '"type":"ADDED"' || { echo "no initial ADDED events"; exit 1; }
echo "$out" | grep -q '"k8s.io/initial-events-end":"true"' || { echo "no initial-events-end bookmark"; exit 1; }
echo "WatchList emitted the initial-events-end bookmark"
