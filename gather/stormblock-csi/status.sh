#!/bin/sh
# gather collector (stormblock-csi): driver/operator pods, wandering CRs,
# capacity objects, leases — the CR + pod state a failover post-mortem needs.
API="${QA_API:-http://127.0.0.1:6443}"
$QA_SSH "echo '== stormblock-system pods =='; wget -qO- '$API/api/v1/namespaces/stormblock-system/pods' 2>/dev/null | head -c 4000
echo; echo '== wanderingvolumes (all ns) =='; wget -qO- '$API/apis/stormblock.io/v1alpha1/wanderingvolumes' 2>/dev/null | head -c 4000
echo; echo '== volumepolicies =='; wget -qO- '$API/apis/stormblock.io/v1alpha1/volumepolicies' 2>/dev/null | head -c 2000
echo; echo '== tiebreak lease =='; wget -qO- '$API/apis/coordination.k8s.io/v1/namespaces/stormblock-system/leases/stormblock-tiebreak' 2>/dev/null
echo; echo '== node leases =='; wget -qO- '$API/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases' 2>/dev/null | head -c 2000
echo; echo '== csistoragecapacities =='; wget -qO- '$API/apis/storage.k8s.io/v1/namespaces/stormblock-system/csistoragecapacities' 2>/dev/null | head -c 2000
echo; echo '== nvme state =='; ls -l /sys/class/nvme 2>/dev/null; nvme list 2>/dev/null
echo '== ublk devices =='; ls -l /dev/ublkb* 2>/dev/null" 2>&1
