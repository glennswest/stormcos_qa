#!/bin/sh
# gather collector (stormblock-csi): driver/operator pods, wandering CRs,
# capacity objects and leases through the cluster API: the CR and pod state
# a failover post-mortem needs. The node's nvme/ublk devices are in
# must-gather's host bundle (host/kernel/).
# must-gather sets QA_API (https), QA_TOKEN_FILE, QA_CA_FILE, QA_INSECURE (#45).
# Owner: glennswest/stormblock-csi.

api() {
    path=$1
    set -- "$QA_API$path"
    [ "${QA_INSECURE:-0}" = 1 ] && set -- -k "$@"
    [ -n "${QA_CA_FILE:-}" ] && set -- --cacert "$QA_CA_FILE" "$@"
    [ -n "${QA_TOKEN_FILE:-}" ] && set -- -H "Authorization: Bearer $(cat "$QA_TOKEN_FILE")" "$@"
    curl -sS --max-time 20 "$@" || echo "(fetch failed: $QA_API$path)"
}

echo '== stormblock-system pods =='; api /api/v1/namespaces/stormblock-system/pods | head -c 4000
echo; echo '== wanderingvolumes (all ns) =='; api /apis/stormblock.io/v1alpha1/wanderingvolumes | head -c 4000
echo; echo '== volumepolicies =='; api /apis/stormblock.io/v1alpha1/volumepolicies | head -c 2000
echo; echo '== tiebreak lease =='; api /apis/coordination.k8s.io/v1/namespaces/stormblock-system/leases/stormblock-tiebreak
echo; echo '== node leases =='; api /apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases | head -c 2000
echo; echo '== csistoragecapacities =='; api /apis/storage.k8s.io/v1/namespaces/stormblock-system/csistoragecapacities | head -c 2000
echo
