#!/bin/sh
# QA-Name: node hostname is set
# QA-Owner: glennswest/stormcos
# QA-Desc: hostname is not localhost — proves systemd-hostnamed could write it under enforcing (stormcos#14) and cloud-init applied it
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 60
set -eu
hn="$($QA_SSH hostname 2>/dev/null | tr -d '[:space:]')"
case "$hn" in
  localhost|localhost.localdomain|"") echo "hostname is '$hn' (not set)"; exit 1 ;;
  *) echo "hostname=$hn" ;;
esac
