#!/usr/bin/env python3
"""Read-only node audit. TURBOMODE_NODES names a JSON node -> SSH argv map."""
import base64
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys


REMOTE = r'''
import base64, json, pathlib, sys, urllib.request
request = json.loads(base64.b64decode(sys.argv[1]))
token = pathlib.Path('/run/stormblock/engine/api_token').read_text().strip()
def get(path):
    req = urllib.request.Request('http://127.0.0.1:9090/api/v1/' + path,
                                 headers={'Authorization': 'Bearer ' + token})
    with urllib.request.urlopen(req, timeout=30) as response:
        value = json.load(response)
        items = value['items']
        if value.get('count', len(items)) != len(items):
            raise RuntimeError('incomplete storage inventory')
        return items
volumes = get('volumes')
owned = [v for v in volumes if v['name'] in request['names'] or v['id'] in request['ids']]
ids = set(request['ids']) | {v['id'] for v in owned}
slabs = get('slabs')
slots = []
for slab in slabs:
    for slot in get('slabs/' + slab['id'] + '/slots'):
        if slot['volume_id'] in ids:
            slots.append(dict(slot, slab=slab['id'], slot_size=slab['slot_size']))
needles = request['uids'] + list(ids) + request['names']
mounts, cgroups = [], []
for proc in pathlib.Path('/proc').iterdir():
    if not proc.name.isdigit():
        continue
    for filename, output in [('mountinfo', mounts), ('cgroup', cgroups)]:
        try:
            lines = (proc / filename).read_text().splitlines()
        except (FileNotFoundError, ProcessLookupError):
            continue
        for line in lines:
            if any(needle in line for needle in needles):
                output.append({'pid': proc.name, 'line': line})
directories = [uid for uid in request['uids'] if (pathlib.Path('/var/lib/kubelet/pods') / uid).exists()]
print(json.dumps({'volumes': owned, 'slots': slots, 'slabs': slabs,
                  'mounts': mounts, 'cgroups': cgroups, 'pod_directories': directories}))
'''


def main():
    phase, report_path = sys.argv[1:]
    path = Path(report_path)
    report = json.loads(path.read_text())
    nodes = json.loads(Path(os.environ["TURBOMODE_NODES"]).read_text())
    expected_nodes = {n["metadata"]["name"] for n in report["nodes"]}
    if not expected_nodes or not expected_nodes.issubset(nodes):
        raise RuntimeError("SSH audit map must cover every cluster node, including storage nodes")
    names = [f"pvc-{c['metadata']['namespace']}-{c['metadata']['name']}" for c in report["claims"]]
    uids = [p["object"]["metadata"]["uid"] for p in report["pods"]]
    uids = list(set(uids) | {p["metadata"]["uid"] for p in report.get("final_pods", [])})
    previous_path = path.with_name("storage-allocated.json")
    previous = json.loads(previous_path.read_text()) if phase == "after" and previous_path.exists() else {}
    ids = [v["id"] for n in previous.get("nodes", {}).values() for v in n["volumes"]]
    # PV handles preserve identities even when an interrupted run never reached
    # its allocated audit. Built-in names also find partially provisioned claims.
    ids += [p["spec"]["csi"]["volumeHandle"] for p in report["pvs"] if p.get("spec", {}).get("csi", {}).get("volumeHandle")]
    encoded = base64.b64encode(json.dumps(dict(names=names, uids=uids, ids=ids)).encode()).decode()
    evidence = {}
    for name, ssh in nodes.items():
        if not isinstance(ssh, list) or not ssh or ssh[0] != "ssh":
            raise RuntimeError("each node entry must be an ssh argument array")
        result = subprocess.run(ssh + ["python3 - " + shlex.quote(encoded)], input=REMOTE,
                                text=True, capture_output=True, timeout=100, check=True)
        evidence[name] = json.loads(result.stdout)
    verified = True
    if phase == "allocated":
        observed = [v for n in evidence.values() for v in n["volumes"]]
        verified = (len(observed) == 100 and {v["name"] for v in observed} == set(names)
                    and all(v["allocated_bytes"] > 0 for v in observed))
    elif phase == "after":
        verified = all(not n[key] for n in evidence.values()
                       for key in ("volumes", "slots", "mounts", "cgroups", "pod_directories"))
    elif phase != "before":
        raise ValueError(phase)
    print(json.dumps({"verified": verified, "phase": phase, "nodes": evidence}))


if __name__ == "__main__":
    main()
