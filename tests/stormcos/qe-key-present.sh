#!/bin/sh
# QA-Name: QE access key present
# QA-Owner: glennswest/stormcos
# QA-Desc: storm has a non-empty authorized_keys and passwordless sudo (the image is reachable)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 30
set -eu
# If this test's $QA_SSH reached the node at all, SSH-as-storm already works.
# But assert the baked artifacts explicitly so a keyless build is caught even if
# the runner reached the node another way — a keyless image (no way in) must
# never ship. See keys/README.md / build-base-rootfs.sh.
$QA_SSH "test -s /home/storm/.ssh/authorized_keys" \
  || { echo "storm authorized_keys missing or empty — image is unreachable (keyless build)"; exit 1; }
$QA_SSH "sudo -n true" \
  || { echo "storm cannot sudo without a password — no root access path"; exit 1; }
echo "QE access key present; storm -> passwordless sudo OK"
