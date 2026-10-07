#!/bin/sh
# gather collector (cadvisor): health, version, machine info, the containers
# cadvisor knows, recent events, its self-metrics, stormd's view of the
# process and its log, and the node's cgroup and block-device trees.
#
# HTTP is fetched from where must-gather runs, straight to the node: cadvisor
# listens on 0.0.0.0:9096 and its stormd on :9196 (port + 100), so nothing is
# needed on the node beyond a shell. Overrides:
#   CADVISOR_ENDPOINT  default http://$QA_NODE_IP:9096 (https once stormcos#143)
#   CADVISOR_TOKEN     bearer token for cadvisor, if it requires one
#   CADVISOR_STORMD    default http://$QA_NODE_IP:9196; "none" to skip
#   STORMD_TOKEN       bearer token for stormd's non-open endpoints
# Run by must-gather where it runs (QA_NODE_IP set), never on the node.
# Owner: glennswest/cadvisor.
NODE="${QA_NODE_IP:-127.0.0.1}"
CAD="${CADVISOR_ENDPOINT:-http://$NODE:9096}"
SD="${CADVISOR_STORMD:-http://$NODE:9196}"

# get <url> [token]: body on stdout; any failure is printed, never fatal.
get() {
    if command -v curl >/dev/null 2>&1; then
        set -- "$1" ${2:+-H "Authorization: Bearer $2"}
        curl -ksS --max-time 20 "$@" || echo "(fetch failed: $1)"
    else
        wget -qO- --no-check-certificate -T 20 ${2:+--header="Authorization: Bearer $2"} "$1" \
            || echo "(fetch failed: $1)"
    fi
}

# keys: the top-level keys of a JSON object, one per line (container names).
keys() {
    if command -v jq >/dev/null 2>&1; then
        jq -r 'keys[]' 2>/dev/null
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c 'import json,sys; print("\n".join(sorted(json.load(sys.stdin))))' 2>/dev/null
    else
        head -c 4000
    fi
}

echo "== cadvisor $CAD =="
for p in /healthz /api/v2.0/version; do
    echo "-- $p"; get "$CAD$p" "${CADVISOR_TOKEN:-}"; echo
done
echo "-- /api/v2.0/attributes"; get "$CAD/api/v2.0/attributes" "${CADVISOR_TOKEN:-}"; echo
echo "-- /api/v2.0/machine"; get "$CAD/api/v2.0/machine" "${CADVISOR_TOKEN:-}"; echo
echo "-- containers (/api/v2.0/spec?recursive=true keys)"
get "$CAD/api/v2.0/spec/?recursive=true" "${CADVISOR_TOKEN:-}" | keys
echo "-- /api/v2.0/events (last 100)"
get "$CAD/api/v2.0/events/?all_events=true&max_events=100" "${CADVISOR_TOKEN:-}"; echo
echo "-- /metrics: self and machine families, series count"
m=$(get "$CAD/metrics" "${CADVISOR_TOKEN:-}")
printf '%s\n' "$m" | grep -E '^(cadvisor_version_info|machine_)' | head -40
echo "container_* series: $(printf '%s\n' "$m" | grep -c '^container_')"

if [ "$SD" != none ]; then
    echo "== stormd $SD =="
    echo "-- /healthz"; get "$SD/healthz"; echo
    echo "-- /metrics (process=cadvisor)"
    get "$SD/metrics" | grep 'process="cadvisor"'
    echo "-- /api/v1/processes/cadvisor"; get "$SD/api/v1/processes/cadvisor" "${STORMD_TOKEN:-}"; echo
    echo "-- /api/v1/logs/cadvisor (tail 300)"
    get "$SD/api/v1/logs/cadvisor?tail=300" "${STORMD_TOKEN:-}"; echo
fi

# The node's cgroup tree, controllers and block-device map (cadvisor#3/#15/#18)
# are in must-gather's host bundle: nodes/<node>/host/cgroup/ and
# host/kernel/block-devices.txt (no ssh to a stormcos node, #45).
