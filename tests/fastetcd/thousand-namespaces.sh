#!/bin/sh
# QA-Name: 1000 namespaces store and read back
# QA-Owner: glennswest/fastetcd
# QA-Desc: fastetcd stores 1000 /registry/namespaces-shaped keys and reads them all back, at the scale a rustkube control plane uses
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 300
set -eu

EP="${FASTETCD_ENDPOINT:-http://127.0.0.1:2379}"
# A prefix unique to this test, so it never touches real namespaces.
PREFIX="/registry/namespaces/qa-1000ns-"
N=1000

# The runner may SIGKILL on timeout, which skips the EXIT trap, so also
# clear leftovers up front. The prefix delete is idempotent and scoped.
cleanup() { $QA_SSH "fastetcd-ctl --endpoint '$EP' del '$PREFIX' --prefix" >/dev/null 2>&1 || true; }
count_keys() {
  $QA_SSH "fastetcd-ctl --endpoint '$EP' get '$PREFIX' --prefix" 2>/dev/null \
    | grep -c "^$PREFIX" || true
}

cleanup
trap cleanup EXIT INT TERM

# Starting state: nothing under our prefix.
start=$(count_keys)
[ "$start" -eq 0 ] || { echo "prefix not empty at start ($start keys)"; exit 1; }

# Create N namespace-shaped keys in one remote loop (single ssh round trip).
# \$i / \$(...) are escaped so they evaluate on the node, not locally.
$QA_SSH "i=1; while [ \$i -le $N ]; do \
  fastetcd-ctl --endpoint '$EP' put ${PREFIX}\$(printf %04d \$i) \
    '{\"kind\":\"Namespace\",\"apiVersion\":\"v1\"}' >/dev/null || exit 1; \
  i=\$((i+1)); done" \
  || { echo "failed writing $N keys"; exit 1; }

# Read them all back.
count=$(count_keys)
[ "$count" -eq "$N" ] || { echo "expected $N namespaces, read back $count"; exit 1; }
echo "wrote and read back $count namespaces"

# Clean up and confirm we are back to the starting state.
$QA_SSH "fastetcd-ctl --endpoint '$EP' del '$PREFIX' --prefix" >/dev/null \
  || { echo "cleanup delete failed"; exit 1; }
after=$(count_keys)
[ "$after" -eq 0 ] || { echo "cleanup left $after keys behind"; exit 1; }
echo "cleaned up; data store back to starting state"
