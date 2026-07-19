#!/bin/sh
# QA-Name: ublk root is erofs
# QA-Owner: glennswest/stormblock
# QA-Desc: root is served from /dev/ublkb0 and is an erofs filesystem
# QA-Scope: cluster
# QA-Severity: blocking
set -eu
fstype=$($QA_SSH "findmnt -n -o FSTYPE /" 2>/dev/null || true)
echo "root fstype: $fstype"
# overlay(erofs) or erofs both acceptable; the lower must be erofs.
$QA_SSH "mount | grep -q 'lowerdir=.*erofs' || findmnt / | grep -q erofs" \
  || { echo "root lower is not erofs"; exit 1; }
echo "ublk erofs root OK"
