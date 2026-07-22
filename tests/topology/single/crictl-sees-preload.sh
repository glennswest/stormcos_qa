#!/bin/sh
# QA-Name: CRI-O sees the preloaded images
# QA-Owner: glennswest/stormcos
# QA-Desc: crictl images lists the preloaded set — proves the image store is wired into CRI-O (stormcos#18), not just mounted
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 90
set -eu
# The store manifest says how many images were preloaded; CRI-O must see > 0.
n="$($QA_SSH "sudo crictl --runtime-endpoint unix:///run/crio/crio.sock images -q 2>/dev/null | wc -l" | tr -d '[:space:]')"
[ "${n:-0}" -gt 0 ] || {
  echo "crictl images sees 0 — image store mounted but NOT wired into CRI-O (stormcos#18)"
  exit 1
}
echo "crictl sees $n images"
