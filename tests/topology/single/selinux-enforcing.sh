#!/bin/sh
# QA-Name: SELinux is enforcing
# QA-Owner: glennswest/stormcos
# QA-Desc: the node runs SELinux in enforcing mode (RHCOS shape), not permissive/disabled
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
mode="$($QA_SSH getenforce 2>/dev/null | tr -d '[:space:]')"
[ "$mode" = "Enforcing" ] || { echo "getenforce=$mode, want Enforcing"; exit 1; }
echo "SELinux Enforcing"
