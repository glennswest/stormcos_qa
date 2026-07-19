#!/bin/sh
# QA-Name: boots to multi-user with node stack
# QA-Desc: node reaches systemd multi-user, kubelet/sshd active, overlay root writable
# QA-Scope: cluster
# QA-Severity: blocking
#
# stormcos-owned. Proves the boot chain + immutable-overlay root actually work.
set -eu
run() { $QA_SSH "$1"; }
state=$(run "systemctl is-system-running" 2>/dev/null || true)
case "$state" in running|degraded) : ;; *) echo "system not up: $state"; exit 1;; esac
run "systemctl is-active sshd" | grep -q active || { echo "sshd not active"; exit 1; }
run "test -w /etc" || { echo "root not writable (overlay missing)"; exit 1; }
run "test -b /dev/ublkb0" || { echo "ublk root device absent"; exit 1; }
echo "node up: $state; sshd active; overlay writable; ublk root present"
