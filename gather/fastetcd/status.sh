#!/bin/sh
# gather collector (fastetcd): service state, health, data-dir size, and
# recent log. Run with the same $QA_SSH env as tests.
#
# The client port :2379 is mutual TLS with the node CA (stormcos#146) and the
# client pairs never leave the node's tier-0, so health comes from the
# metrics port instead: plain HTTP on the node's loopback, etcd's metric
# names (`etcd_server_has_leader` 1 = healthy). #51
METRICS="${FASTETCD_METRICS_ENDPOINT:-http://127.0.0.1:2381}"
DATA="${FASTETCD_DATA_DIR:-/var/lib/fastetcd/data}"
$QA_SSH "echo '== fastetcd unit =='; systemctl status fastetcd --no-pager 2>/dev/null
echo '== health (metrics :2381) =='; wget -qO- '$METRICS/metrics' 2>/dev/null | grep -E '^etcd_(server_has_leader|server_leader_changes_seen|mvcc_db_total_size_in_bytes)' || echo '(metrics not answering)'
echo '== data dir =='; ls -lh '$DATA' 2>/dev/null; du -sh '$DATA' 2>/dev/null
echo '== backups =='; ls -lh '$DATA/backups' 2>/dev/null
echo '== fsck =='; fastetcd --data-dir '$DATA' fsck 2>/dev/null || echo '(fsck needs the server stopped)'
echo '== recent journal =='; journalctl -u fastetcd --no-pager -n 200 2>/dev/null" 2>&1
