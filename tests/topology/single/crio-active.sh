#!/bin/sh
# QA-Name: CRI-O is active
# QA-Owner: glennswest/stormcos
# QA-Desc: crio.service is running and answers CRI v1 on /run/crio/crio.sock
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
st="$($QA_SSH systemctl is-active crio 2>/dev/null | tr -d '[:space:]')"
[ "$st" = "active" ] || { echo "crio is $st"; exit 1; }
$QA_SSH "sudo crictl --runtime-endpoint unix:///run/crio/crio.sock version" 2>/dev/null \
  | grep -q "RuntimeName" || { echo "crictl could not reach CRI-O"; exit 1; }
echo "CRI-O active + CRI v1"
