#!/bin/sh
# QA-Name: ublk root + writable volumes present
# QA-Owner: glennswest/stormblock
# QA-Desc: /dev/ublkb0..3 exist (root, image-store, /var, /var/lib/containers)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
for d in 0 1 2 3; do
  $QA_SSH "test -b /dev/ublkb$d" || { echo "/dev/ublkb$d missing"; exit 1; }
done
echo "ublkb0..3 present"
