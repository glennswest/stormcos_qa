#!/bin/sh
# QA-Name: HA read-your-write across masters
# QA-Owner: glennswest/rustkube
# QA-Desc: an object created via one master's apiserver is immediately readable via another master's apiserver (shared fastetcd, HA control plane)
# QA-Scope: cluster
# QA-Topology: full
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
set -- $QA_MASTERS
M1=$1; M2=${2:-$1}
N="qa-ha-$$"
cleanup() { curl -s -o /dev/null -X DELETE "http://$M1:6443/api/v1/namespaces/$N" >/dev/null 2>&1 || true; }
cleanup; trap cleanup EXIT INT TERM
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"'"$N"'"}}' "http://$M1:6443/api/v1/namespaces")
[ "$code" = 201 ] || { echo "create via M1 ($M1) got $code"; exit 1; }
curl -s "http://$M2:6443/api/v1/namespaces/$N" | grep -q "\"name\":\"$N\"" || { echo "not readable via M2 ($M2)"; exit 1; }
echo "write via $M1 -> read via $M2 (shared store HA)"
