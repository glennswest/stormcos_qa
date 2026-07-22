#!/bin/sh
# QA-Name: preloaded image store mounted
# QA-Owner: glennswest/stormcos
# QA-Desc: the erofs image store is mounted read-only at /var/lib/stormcos/image-store with its manifest present
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
$QA_SSH "mount | grep -q '/var/lib/stormcos/image-store type erofs'" \
  || { echo "image store not mounted (erofs)"; exit 1; }
$QA_SSH "test -f /var/lib/stormcos/image-store/image-store.json" \
  || { echo "image-store.json missing"; exit 1; }
echo "image store mounted"
