#!/bin/sh
# QA-Name: as=PartialObjectMetadata projection
# QA-Owner: glennswest/rustkube
# QA-Desc: a list with Accept application/json;as=PartialObjectMetadata returns a PartialObjectMetadataList of metadata-only items — what metadata informers (Cilium on CRDs) need (#39 chain)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
exec $QA_SSH sh <<'REMOTE'
set -eu
API=http://127.0.0.1:6443
out=$(curl -s -H 'Accept: application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1' \
  "$API/api/v1/namespaces?limit=1" 2>/dev/null || true)
echo "$out" | grep -q '"kind":"PartialObjectMetadataList"' \
  || { echo "not projected to PartialObjectMetadataList: $out"; exit 1; }
echo "$out" | grep -q '"kind":"PartialObjectMetadata"' \
  || { echo "items not PartialObjectMetadata: $out"; exit 1; }
# metadata-only: no spec/status on the projected item
echo "$out" | grep -q '"spec"' \
  && { echo "projection leaked spec (should be metadata only): $out"; exit 1; }
echo "as=PartialObjectMetadata projection correct"
REMOTE
