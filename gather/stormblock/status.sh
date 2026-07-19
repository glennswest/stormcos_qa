#!/bin/sh
# gather collector (stormblock): volumes, slab, and target-server state.
$QA_SSH "echo '== stormblock units =='; systemctl status stormblock stormblock-target --no-pager 2>/dev/null
echo '== volumes.dat =='; ls -l /etc/stormblock/meta/ 2>/dev/null
echo '== ublk root =='; findmnt / ; mount | grep -Ei 'erofs|overlay|ublk'" 2>&1
