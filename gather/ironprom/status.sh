#!/bin/sh
# gather collector (ironprom): its pods and StatefulSet through the cluster
# API, then the live TSDB, targets and buildinfo from its HTTP API when the
# pod's address is reachable from where must-gather runs.
# must-gather sets QA_API (https), QA_TOKEN_FILE, QA_CA_FILE, QA_INSECURE (#45).
# Owner: glennswest/ironprom.
NS="${IRONPROM_NAMESPACE:-monitoring}"

api() {
    path=$1
    set -- "$QA_API$path"
    [ "${QA_INSECURE:-0}" = 1 ] && set -- -k "$@"
    [ -n "${QA_CA_FILE:-}" ] && set -- --cacert "$QA_CA_FILE" "$@"
    [ -n "${QA_TOKEN_FILE:-}" ] && set -- -H "Authorization: Bearer $(cat "$QA_TOKEN_FILE")" "$@"
    curl -sS --max-time 20 "$@" || echo "(fetch failed: $QA_API$path)"
}

echo "== ironprom pods ($NS) =="
pods=$(api "/api/v1/namespaces/$NS/pods")
echo "$pods" | tr ',' '\n' | grep -Ei 'ironprom|"phase"|"ready"|podIP' | head -40
echo "== ironprom statefulsets ($NS) =="
api "/apis/apps/v1/namespaces/$NS/statefulsets" | head -c 4000; echo

ip=$(echo "$pods" | grep -oE '"podIP": *"[0-9.]+"' | head -1 | grep -oE '[0-9.]+$')
[ -n "$ip" ] || { echo "(no ironprom pod address)"; exit 0; }
EP="http://$ip:9090"
get() { curl -sS --max-time 10 "$EP$1" 2>&1 || echo "(not reachable from here: $EP$1)"; }
echo "== ironprom endpoint: $EP =="
echo '== buildinfo =='; get /api/v1/status/buildinfo
echo; echo '== status/tsdb =='; get /api/v1/status/tsdb
echo; echo '== status/runtimeinfo =='; get /api/v1/status/runtimeinfo
echo; echo '== targets =='; get /api/v1/targets
echo; echo '== rules =='; get /api/v1/rules
echo; echo '== self-metrics (ironprom_*) =='; get /metrics | grep '^ironprom_'
