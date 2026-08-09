#!/bin/sh
# QA-Name: PVC provisions a stormblock volume and a pod writes through it
# QA-Owner: glennswest/stormblock-csi
# QA-Desc: WFFC PVC + writer pod: pod Succeeded proves provision -> attach -> stage -> mount -> write; PVC Bound; WanderingVolume CR mirrored
# QA-Scope: cluster
# QA-Severity: warn
# QA-Timeout: 300
set -eu

# warn (not blocking): a single-node QA cluster cannot satisfy StormBlock's
# mandatory master/slave anti-affinity (the pair needs two nodes), so
# provisioning may legitimately fail there. The filed issue tells us which.

API="${QA_API:-http://127.0.0.1:6443}"
NS=qa-stormblock-csi

api_post() { path=$1; data=$2; $QA_SSH "wget -qO- --header='Content-Type: application/json' --post-data='$data' '$API$path'"; }
api_get()  { $QA_SSH "wget -qO- '$API$1'" 2>/dev/null || true; }
api_del()  { # busybox wget cannot DELETE; try curl, then GNU wget --method.
  $QA_SSH "curl -sf -X DELETE '$API$1' >/dev/null 2>&1 || wget -qO- --method=DELETE '$API$1' >/dev/null 2>&1" || true
}

cleanup() {
  api_del "/api/v1/namespaces/$NS/pods/qa-writer"
  api_del "/api/v1/namespaces/$NS/persistentvolumeclaims/qa-pvc"
  api_del "/api/v1/namespaces/$NS"
}
cleanup
trap cleanup EXIT INT TERM

api_post /api/v1/namespaces \
  '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"'$NS'"}}' >/dev/null || true

api_post "/api/v1/namespaces/$NS/persistentvolumeclaims" '{
  "apiVersion":"v1","kind":"PersistentVolumeClaim",
  "metadata":{"name":"qa-pvc","namespace":"'$NS'"},
  "spec":{"accessModes":["ReadWriteOnce"],"storageClassName":"stormblock",
          "resources":{"requests":{"storage":"1Gi"}}}}' >/dev/null

# restartPolicy Never + exit 0 after the write: phase Succeeded proves the
# volume mounted AND accepted a write — no exec channel needed.
api_post "/api/v1/namespaces/$NS/pods" '{
  "apiVersion":"v1","kind":"Pod",
  "metadata":{"name":"qa-writer","namespace":"'$NS'"},
  "spec":{"restartPolicy":"Never",
    "containers":[{"name":"writer","image":"busybox",
      "command":["sh","-c","echo stormblock-qa-proof > /data/proof && cat /data/proof"],
      "volumeMounts":[{"name":"data","mountPath":"/data"}]}],
    "volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"qa-pvc"}}]}}' >/dev/null

i=0
while [ $i -lt 48 ]; do
  pod=$(api_get "/api/v1/namespaces/$NS/pods/qa-writer")
  printf '%s' "$pod" | grep -q '"phase":"Succeeded"' && break
  if printf '%s' "$pod" | grep -q '"phase":"Failed"'; then
    echo "writer pod failed:"; printf '%s\n' "$pod" | head -c 600; exit 1
  fi
  i=$((i+1)); sleep 5
done
printf '%s' "$pod" | grep -q '"phase":"Succeeded"' \
  || { echo "writer pod never completed (WFFC bind, attach, or mount stuck):"; printf '%s\n' "$pod" | head -c 600; exit 1; }

api_get "/api/v1/namespaces/$NS/persistentvolumeclaims/qa-pvc" | grep -q '"phase":"Bound"' \
  || { echo "PVC not Bound after pod completion"; exit 1; }

# The wander operator mirrors PVs of our driver into WanderingVolume CRs
# named after the PVC.
api_get "/apis/stormblock.io/v1alpha1/namespaces/$NS/wanderingvolumes/qa-pvc" \
  | grep -q '"volumeId"' \
  || { echo "no WanderingVolume CR mirrored for qa-pvc (operator not steering this volume)"; exit 1; }

echo "pvc provisioned, pod wrote through the volume, wandering CR mirrored"
