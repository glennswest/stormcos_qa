#!/bin/sh
# gather collector (kernel): ublk + io_uring runtime state — the two things
# that gate stormblock's ublk root on RHEL10.
$QA_SSH "echo '== io_uring_disabled =='; cat /proc/sys/kernel/io_uring_disabled 2>/dev/null
echo '== ublk devices =='; ls -l /dev/ublk* 2>/dev/null
echo '== ublk_drv =='; lsmod | grep -i ublk
echo '== ublk sysfs =='; ls /sys/class/ublk-char 2>/dev/null; cat /sys/kernel/debug/ublk/* 2>/dev/null" 2>&1
