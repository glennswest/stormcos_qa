#!/usr/bin/env python3
"""Read-only stormblock audit, run on the node itself (owner, #26).

It runs inside a test Job on the node, never over ssh. The Job declares its
host access read-only: hostPID, hostPath /proc, /sys/fs/cgroup,
/proc/1/mountinfo and only the file /run/stormblock/engine/api_token. See
README.md. Paths and the stormblock URL come from the environment:

  TURBOMODE_NODE            the node this runs on (downward API spec.nodeName); required
  TURBOMODE_PROC            host /proc (default /proc; with hostPID it is the host's)
  TURBOMODE_HOST_MOUNTINFO  host init's mountinfo (default <proc>/1/mountinfo)
  TURBOMODE_CGROUP          host cgroup tree (default /sys/fs/cgroup)
  TURBOMODE_TOKEN           stormblock engine token file (default /run/stormblock/engine/api_token)
  TURBOMODE_STORMBLOCK      stormblock API (default http://$STORM_NODE:9090, else http://127.0.0.1:9090)
  TURBOMODE_KUBELET_PODS    optional kubelet pods dir; not mounted means unmeasured, recorded as such

One Job sees one node, so the cluster must have exactly that node: anything
else is an incomplete inventory and fails.
"""
import json
import os
from pathlib import Path
import sys
import urllib.request


def env_path(name, default):
    return Path(os.environ.get(name) or default)


def stormblock(base, token):
    def get(path):
        req = urllib.request.Request(base.rstrip("/") + "/api/v1/" + path,
                                     headers={"Authorization": "Bearer " + token})
        with urllib.request.urlopen(req, timeout=30) as response:
            value = json.load(response)
        items = value["items"]
        if value.get("count", len(items)) != len(items):
            raise RuntimeError(f"incomplete storage inventory at {path}")
        return items
    return get


def matching_lines(path, needles, pid, output):
    try:
        lines = path.read_text().splitlines()
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return  # the process exited, or is not ours to read: nothing to match
    for line in lines:
        if any(needle in line for needle in needles):
            output.append({"pid": pid, "line": line})


def cgroup_dirs(root, needles, depth=10):
    """Cgroup directories naming a test Pod or volume. The kubelet writes a Pod
    UID with underscores in systemd-style slice names, so both forms count."""
    found = []
    def walk(directory, level):
        try:
            children = [c for c in directory.iterdir() if c.is_dir()]
        except (FileNotFoundError, PermissionError):
            return
        for child in children:
            if any(needle in child.name for needle in needles):
                found.append(str(child.relative_to(root)))
            elif level < depth:
                walk(child, level + 1)
    if not root.is_dir():
        raise RuntimeError(f"cgroup tree {root} is not mounted")
    walk(root, 0)
    return found


def collect(request):
    proc = env_path("TURBOMODE_PROC", "/proc")
    host_mountinfo = env_path("TURBOMODE_HOST_MOUNTINFO", proc / "1" / "mountinfo")
    cgroup = env_path("TURBOMODE_CGROUP", "/sys/fs/cgroup")
    token = env_path("TURBOMODE_TOKEN", "/run/stormblock/engine/api_token").read_text().strip()
    node = os.environ.get("STORM_NODE")
    base = os.environ.get("TURBOMODE_STORMBLOCK") or f"http://{node or '127.0.0.1'}:9090"
    get = stormblock(base, token)
    volumes = get("volumes")
    owned = [v for v in volumes if v["name"] in request["names"] or v["id"] in request["ids"]]
    ids = set(request["ids"]) | {v["id"] for v in owned}
    slabs = get("slabs")
    slots = []
    for slab in slabs:
        for slot in get("slabs/" + slab["id"] + "/slots"):
            if slot["volume_id"] in ids:
                slots.append(dict(slot, slab=slab["id"], slot_size=slab["slot_size"]))
    needles = request["uids"] + sorted(ids) + request["names"]
    if not host_mountinfo.is_file():
        raise RuntimeError(f"host mountinfo {host_mountinfo} is not mounted")
    if not (proc / "1").is_dir():
        raise RuntimeError(f"{proc} shows no pid 1: hostPID or the /proc mount is missing")
    mounts, cgroups = [], []
    matching_lines(host_mountinfo, needles, "host", mounts)
    for entry in proc.iterdir():
        if entry.name.isdigit():
            matching_lines(entry / "mountinfo", needles, entry.name, mounts)
            matching_lines(entry / "cgroup", needles, entry.name, cgroups)
    cgroup_needles = needles + [uid.replace("-", "_") for uid in request["uids"]]
    evidence = {"volumes": owned, "slots": slots, "slabs": slabs, "mounts": mounts,
                "cgroups": cgroups, "cgroup_dirs": cgroup_dirs(cgroup, cgroup_needles),
                "pod_directories": None, "unmeasured": []}
    pods = os.environ.get("TURBOMODE_KUBELET_PODS")
    if pods and Path(pods).is_dir():
        evidence["pod_directories"] = [uid for uid in request["uids"] if (Path(pods) / uid).exists()]
    else:
        evidence["unmeasured"].append("pod_directories")
    return evidence


def main():
    phase, report_path = sys.argv[1:]
    if phase not in ("before", "allocated", "after"):
        raise ValueError(phase)
    path = Path(report_path)
    report = json.loads(path.read_text())
    here = os.environ.get("TURBOMODE_NODE")
    if not here:
        raise RuntimeError("TURBOMODE_NODE (the node this audit runs on) is not set")
    nodes = {n["metadata"]["name"] for n in report["nodes"]}
    if nodes != {here}:
        raise RuntimeError(f"the audit sees only node {here}; the cluster has {sorted(nodes)}")
    names = [f"pvc-{c['metadata']['namespace']}-{c['metadata']['name']}" for c in report["claims"]]
    uids = [p["object"]["metadata"]["uid"] for p in report["pods"]]
    uids = sorted(set(uids) | {p["metadata"]["uid"] for p in report.get("final_pods", [])})
    previous_path = path.with_name("storage-allocated.json")
    previous = json.loads(previous_path.read_text()) if phase == "after" and previous_path.exists() else {}
    ids = [v["id"] for n in previous.get("nodes", {}).values() for v in n["volumes"]]
    # PV handles preserve identities even when an interrupted run never reached
    # its allocated audit. Built-in names also find partially provisioned claims.
    ids += [p["spec"]["csi"]["volumeHandle"] for p in report["pvs"] if p.get("spec", {}).get("csi", {}).get("volumeHandle")]
    evidence = {here: collect(dict(names=names, uids=uids, ids=ids))}
    verified = True
    if phase == "allocated":
        observed = [v for n in evidence.values() for v in n["volumes"]]
        verified = (len(observed) == 100 and {v["name"] for v in observed} == set(names)
                    and all(v["allocated_bytes"] > 0 for v in observed))
    elif phase == "after":
        verified = all(not n[key] for n in evidence.values()
                       for key in ("volumes", "slots", "mounts", "cgroups", "cgroup_dirs", "pod_directories"))
    print(json.dumps({"verified": verified, "phase": phase, "nodes": evidence}))


if __name__ == "__main__":
    main()
