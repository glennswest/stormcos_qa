#!/bin/sh
# QA-Name: ublk root is erofs
# QA-Owner: glennswest/stormblock
# QA-Desc: root is served from /dev/ublkb0 and is an erofs filesystem
# QA-Scope: cluster
# QA-Severity: blocking
set -eu
fstype=$($QA_SSH "findmnt -n -o FSTYPE /" 2>/dev/null || true)
echo "root fstype: $fstype"
# root is overlay(erofs lower) or erofs directly. The overlay records its lower
# only as a PATH (lowerdir=/run/stormblock/lower), so the erofs check must look
# at the backing mount: root=/dev/ublkb0 on the cmdline is mounted erofs.
$QA_SSH "grep -qE '^/dev/ublkb0 .+ erofs ' /proc/mounts" \
  || { echo "root lower (/dev/ublkb0) is not erofs"; exit 1; }
echo "ublk erofs root OK"
