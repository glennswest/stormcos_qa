#!/bin/sh
# QA-Name: as=PartialObjectMetadata projection
# QA-Owner: glennswest/rustkube
# QA-Desc: a list with Accept application/json;as=PartialObjectMetadata returns a PartialObjectMetadataList of metadata-only items (#39 chain)
# QA-Scope: cluster
# QA-Severity: blocking
# QA-Timeout: 60
set -eu
API="http://${QA_NODE_IP}:6443"
out=$(curl -s -H 'Accept: application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1' "$API/api/v1/namespaces?limit=1")
echo "$out" | grep -q '"kind":"PartialObjectMetadataList"' || { echo "not projected: $out"; exit 1; }
echo "$out" | grep -q '"kind":"PartialObjectMetadata"' || { echo "items not PartialObjectMetadata"; exit 1; }
echo "$out" | grep -q '"spec"' && { echo "projection leaked spec"; exit 1; }
echo "as=PartialObjectMetadata projection correct"
