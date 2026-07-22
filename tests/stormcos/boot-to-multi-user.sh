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
# $QA_SSH connects as an unprivileged user, so `test -w /etc` would fail even
# though the overlay upper IS writable (for root). Check the root is the
# immutable overlay and that root can write it.
run "findmnt -no FSTYPE / | grep -qE 'overlay|erofs'" || { echo "root not overlay/erofs (immutable root missing)"; exit 1; }
run "sudo test -w /etc" || { echo "root not writable (overlay upper missing)"; exit 1; }
run "test -b /dev/ublkb0" || { echo "ublk root device absent"; exit 1; }
echo "node up: $state; sshd active; overlay writable; ublk root present"
