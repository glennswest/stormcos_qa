#!/bin/sh
# QA-Name: DNS resolves
# QA-Owner: glennswest/stormcos
# QA-Desc: /etc/resolv.conf is populated and a hostname resolves — proves SELinux relabel let NetworkManager write resolv.conf (stormcos#14)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
$QA_SSH "grep -q '^nameserver' /etc/resolv.conf" \
  || { echo "/etc/resolv.conf has no nameserver (SELinux blocked NetworkManager? #14)"; exit 1; }
$QA_SSH "getent hosts github.com >/dev/null 2>&1 || getent hosts pve.g8.lo >/dev/null 2>&1" \
  || { echo "DNS resolution failed"; exit 1; }
echo "DNS resolves"
