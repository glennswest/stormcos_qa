#!/bin/sh
# gather collector (network, #22): the pod network's state for one node: the
# Network CR, the Cilium and network-operator workloads, cilium-config, this
# node's Cilium agent pod, CiliumNode and endpoints, LB pools and BGP
# objects, and the agent's metrics. Everything else is in the bundle already:
#   cluster/crs/networks.network.storm.io.json, cluster/crs/*.cilium.io.json
#   logs/kube-system/cilium-*, cilium-operator-*, network-operator-*
#   nodes/<node>/host/cilium/   the agent's own API: status.json is what
#                               `cilium status --verbose` shows (no exec on a
#                               node, stormpump#103), plus config, endpoints,
#                               cluster-nodes, maps
#   nodes/<node>/host/net/      the host's /proc/net (routes, devices)
# must-gather sets QA_API (https), QA_TOKEN_FILE, QA_CA_FILE, QA_INSECURE,
# QA_NODE, QA_NODE_IP (#45). python3 summarises the JSON when present;
# without it the JSON is printed as it came. Overrides:
#   CILIUM_NAMESPACE  default kube-system
#   CILIUM_METRICS    default http://$QA_NODE_IP:9962/metrics; "none" to skip
# Owner: glennswest/network-operator (Cilium itself: stormcos-cilium).
NS="${CILIUM_NAMESPACE:-kube-system}"
NODE="${QA_NODE:-}"
NODE_IP="${QA_NODE_IP:-127.0.0.1}"
METRICS="${CILIUM_METRICS:-http://$NODE_IP:9962/metrics}"

api() {
    path=$1
    set -- "$QA_API$path"
    [ "${QA_INSECURE:-0}" = 1 ] && set -- -k "$@"
    [ -n "${QA_CA_FILE:-}" ] && set -- --cacert "$QA_CA_FILE" "$@"
    [ -n "${QA_TOKEN_FILE:-}" ] && set -- -H "Authorization: Bearer $(cat "$QA_TOKEN_FILE")" "$@"
    curl -sS --max-time 20 --retry 2 --retry-delay 1 --retry-max-time 30 --retry-connrefused "$@" || echo "(fetch failed: $QA_API$path)"
}

# summ <mode> [arg]: a JSON object or list on stdin, summarised.
#   status       each object's name, spec.ipam if any, and .status (or the
#                API's error message)
#   pods         name, node, phase, ready, restarts per pod
#   names        count, then the names
#   on-node IP   CiliumEndpoints whose status.networking.node is IP
#   redact       a ConfigMap's data, values of secret-looking keys replaced
#   cilium-crds  list paths of the cilium.io CRDs for LB pools and BGP
summ() {
    if ! command -v python3 >/dev/null 2>&1; then
        head -c 20000; echo; return
    fi
    python3 -c '
import json, sys
mode, arg = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else "")
raw = sys.stdin.read()
try:
    v = json.loads(raw)
except ValueError:
    print(raw[:4000]); sys.exit(0)
if v.get("kind") == "Status":
    print("(%s %s)" % (v.get("code", ""), v.get("message", ""))); sys.exit(0)
items = v.get("items") if isinstance(v.get("items"), list) else [v]
name = lambda o: o.get("metadata", {}).get("name", "?")
if mode == "status":
    for o in items:
        print("-- %s" % name(o))
        if o.get("spec", {}).get("ipam"):
            print("spec.ipam: " + json.dumps(o["spec"]["ipam"], sort_keys=True))
        print(json.dumps(o.get("status", {}), indent=2, sort_keys=True))
elif mode == "pods":
    for p in items:
        st = p.get("status", {})
        cs = st.get("initContainerStatuses", []) + st.get("containerStatuses", [])
        ready = all(c.get("ready") for c in st.get("containerStatuses", [])) if st.get("containerStatuses") else False
        print("%s node=%s phase=%s ready=%s restarts=%d" % (name(p), p.get("spec", {}).get("nodeName", ""), st.get("phase", ""), ready, sum(c.get("restartCount", 0) for c in cs)))
        for c in cs:
            w = c.get("state", {}).get("waiting") or c.get("lastState", {}).get("terminated")
            if w:
                print("   %s: %s %s" % (c.get("name"), w.get("reason", ""), (w.get("message") or "")[:300]))
elif mode == "names":
    print("%d: %s" % (len(items), " ".join(name(o) for o in items)))
elif mode == "on-node":
    mine = [o for o in items if o.get("status", {}).get("networking", {}).get("node") == arg]
    print("%d of %d endpoints on %s" % (len(mine), len(items), arg))
    for o in mine:
        st = o.get("status", {})
        addrs = ",".join(a.get("ipv4") or a.get("ipv6") or "" for a in st.get("networking", {}).get("addressing", []))
        print("  %s/%s id=%s state=%s %s" % (o.get("metadata", {}).get("namespace", ""), name(o), st.get("id", ""), st.get("state", ""), addrs))
elif mode == "redact":
    words = ("token", "secret", "password", "passwd", "credential", "private", "apikey", "api-key", "api_key")
    for k, val in sorted((v.get("data") or {}).items()):
        print("%s: %s" % (k, "<redacted>" if any(w in k.lower() for w in words) else val))
elif mode == "cilium-crds":
    for o in items:
        s = o.get("spec", {})
        plural = s.get("names", {}).get("plural", "")
        if s.get("group") != "cilium.io" or not ("loadbalancerippool" in plural or "bgp" in plural):
            continue
        vs = s.get("versions", [])
        ver = next((x["name"] for x in vs if x.get("storage")), vs[0]["name"] if vs else "v2")
        print("/apis/cilium.io/%s/%s" % (ver, plural))
' "$@"
}

echo "== Network CR (networks.network.storm.io) =="
api /apis/network.storm.io/v1/networks | summ status

echo; echo "== workloads ($NS) =="
for w in apps/v1/daemonsets/cilium apps/v1/deployments/cilium-operator apps/v1/daemonsets/cilium-envoy apps/v1/deployments/network-operator; do
    echo "-- $w"
    api "/apis/${w%%/*}/$(echo "$w" | cut -d/ -f2)/namespaces/$NS/$(echo "$w" | cut -d/ -f3-)" | summ status
done

echo; echo "== ConfigMap $NS/cilium-config (secret-looking keys redacted) =="
api "/api/v1/namespaces/$NS/configmaps/cilium-config" | summ redact

echo; echo "== Cilium agent pod on ${NODE:-?} =="
api "/api/v1/namespaces/$NS/pods?labelSelector=k8s-app%3Dcilium&fieldSelector=spec.nodeName%3D$NODE" | summ pods
echo "(its log: logs/$NS/<pod>/; the agent's own status: nodes/$NODE/host/cilium/status.json)"
echo; echo "== operator pods =="
api "/api/v1/namespaces/$NS/pods?labelSelector=io.cilium%2Fapp%3Doperator" | summ pods
api "/api/v1/namespaces/$NS/pods?labelSelector=app.kubernetes.io%2Fname%3Dnetwork-operator" | summ pods

echo; echo "== CiliumNode $NODE =="
api "/apis/cilium.io/v2/ciliumnodes/$NODE" | summ status
echo; echo "== CiliumEndpoints on $NODE_IP =="
api /apis/cilium.io/v2/ciliumendpoints | summ on-node "$NODE_IP"

echo; echo "== LB IP pools and BGP objects =="
paths=$(api /apis/apiextensions.k8s.io/v1/customresourcedefinitions | summ cilium-crds)
[ -n "$paths" ] || echo "(no CiliumLoadBalancerIPPool / CiliumBGP* CRDs)"
for p in $paths; do
    echo "-- $p"
    api "$p" | summ status
done

if [ "$METRICS" != none ]; then
    echo; echo "== agent metrics $METRICS (health, IPAM, endpoints, errors) =="
    m=$(curl -sS --max-time 20 --retry 2 --retry-delay 1 --retry-max-time 30 --retry-connrefused "$METRICS" 2>&1) || m="(fetch failed: $METRICS)"
    printf '%s\n' "$m" | grep -E '^(cilium_(version|unreachable_nodes|unreachable_health_endpoints|endpoint_state|endpoint_regenerations_total|ip_addresses|ipam_|errors_warnings_total|controllers_failing|policy_import_errors|datapath_errors_total|bpf_map_pressure|k8s_client_api_calls_total))|fetch failed' | head -80
fi
