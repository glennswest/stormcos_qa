#!/bin/sh
# gather collector (fastetcd): service state, health, data-dir size, and
# recent log. Run with the same $QA_SSH env as tests.
EP="${FASTETCD_ENDPOINT:-http://127.0.0.1:2379}"
DATA="${FASTETCD_DATA_DIR:-/var/lib/fastetcd/data}"
$QA_SSH "echo '== fastetcd unit =='; systemctl status fastetcd --no-pager 2>/dev/null
echo '== /health =='; wget -qO- '$EP/health' 2>/dev/null; echo
echo '== data dir =='; ls -lh '$DATA' 2>/dev/null; du -sh '$DATA' 2>/dev/null
echo '== backups =='; ls -lh '$DATA/backups' 2>/dev/null
echo '== fsck =='; fastetcd --data-dir '$DATA' fsck 2>/dev/null || echo '(fsck needs the server stopped)'
echo '== recent journal =='; journalctl -u fastetcd --no-pager -n 200 2>/dev/null" 2>&1
