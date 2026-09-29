# Turbomode cluster load acceptance

Explicit stress tests, outside the automatically discovered per-release suite.
Do not run both profiles concurrently. These require cluster-admin access and
enough capacity for the requested workload; the runner does not reduce counts.

1. `sleep`: 100 namespaces, 10 single-container Pods per namespace (1,000 total),
   each running `sleep 120`, restart policy Never.
2. `sqlite`: 100 namespaces, one single-container Pod and one fresh PVC each.
   Each writes 1,000 deterministic, Pod-specific 1 KiB records in a FULL-sync
   transaction. It closes/reopens SQLite, reads and compares every record and
   SHA-256 checksum, runs `PRAGMA integrity_check`, sleeps 120 seconds, then
   reopens and verifies everything again. No dependency installation at startup.

Select the target explicitly and start an authenticated local proxy:

```sh
kubectl --context TEST_CONTEXT proxy --port=18001
python3 tools/turbomode/run.py sleep --api http://127.0.0.1:18001 \
  --cluster TEST_MACHINE_RELEASE --image busybox --out /tmp/turbo-sleep
python3 tools/turbomode/run.py sqlite \
  --api http://127.0.0.1:18001 --cluster TEST_MACHINE_RELEASE \
  --image PYTHON_IMAGE_WITH_SQLITE --storage-class stormblock \
  --storage-audit tools/turbomode/stormblock-audit.py --out /tmp/turbo-sqlite
```

Use the installed release's warm images and provisioned blank templates for a
warm run; identify cold runs separately. The Python image must provide Python 3
and its standard sqlite3 module. Pin image digests in recorded acceptance runs.
The SQLite audit runs **on the node**, never over ssh (see below).

## The node audit (owner, #26)

`stormblock-audit.py` runs on the node inside a test Job that stormcentral's
runner starts (`stormcentral test run stormcos_qa …`) in the run's own
namespace; results come back in the pod log. The Job's host access is
declared and **read-only**, and nothing on the host is writable:

```yaml
spec:
  serviceAccountName: storm-test        # Role limited to the run namespace
  hostPID: true
  containers:
  - name: test
    env:
    - {name: TURBOMODE_NODE, valueFrom: {fieldRef: {fieldPath: spec.nodeName}}}
    - {name: TURBOMODE_PROC, value: /host/proc}
    - {name: TURBOMODE_HOST_MOUNTINFO, value: /host/mountinfo}
    - {name: TURBOMODE_CGROUP, value: /host/cgroup}
    - {name: TURBOMODE_TOKEN, value: /host/stormblock-token}
    volumeMounts:
    - {name: proc, mountPath: /host/proc, readOnly: true}
    - {name: mountinfo, mountPath: /host/mountinfo, readOnly: true}
    - {name: cgroup, mountPath: /host/cgroup, readOnly: true}
    - {name: token, mountPath: /host/stormblock-token, readOnly: true}
  volumes:
  - {name: proc, hostPath: {path: /proc}}
  - {name: mountinfo, hostPath: {path: /proc/1/mountinfo, type: File}}
  - {name: cgroup, hostPath: {path: /sys/fs/cgroup}}
  - {name: token, hostPath: {path: /run/stormblock/engine/api_token, type: File}}  # the file only, not /run
```

It reads stormblock at `http://$STORM_NODE:9090` (override:
`TURBOMODE_STORMBLOCK`) with the token, scans every host process's
mountinfo and cgroup plus host init's mountinfo, and walks the cgroup tree
for directories naming a test Pod (UID with dashes or underscores) or
volume. The kubelet's per-Pod directories are not in the mount list, so
they are recorded as `unmeasured: ["pod_directories"]` (set
`TURBOMODE_KUBELET_PODS` to a mounted directory to check them). One Job sees
one node: if the cluster has any other node, the audit fails. The token
never enters a report. The same Job serves the Supermicro blades later.

*Not yet in the runner:* its Job spec is fixed (stormcentral#74: no
hostPID or hostPath opt-in), so this Job cannot be started yet.

The runner records request-to-observed-scheduling/Running/completion p50/p95/p99/
max, API acknowledgement latency, sample counts, observed peak concurrent
Running Pods, exact UIDs, final status, and per-container SQLite evidence.
Watch observations use client monotonic time, not second-granularity server
timestamps. Watch recovery is recorded and invalidates latency conclusions.
Missing Running observations stay missing; never interpret them as zero latency.
These are end-to-end observed latencies, not a claim to isolate server execution.

Cleanup always runs, including after workload failure: delete run namespaces
with UID preconditions and wait for Pods, PVCs, PVs and VolumeAttachments to
disappear. Never remove finalizers or delete backing storage directly to make a
test pass. The storage class must use Delete; shared classes are never modified.
SQLite success additionally requires a backend audit before allocation, while
all 100 PVCs still exist, and after cleanup. The built-in stormblock auditor
records private allocated bytes and slab free space, then checks **all slab
allocation records** for surviving test-volume owners, all process mount/cgroup
state, and per-Pod directories. Shared template extents are deliberately retained.
This verifies allocation reclamation, not secure erasure of physical media.

Backend audits are read-only executables receiving `PHASE REPORT_JSON`, returning
JSON with `verified: true` and raw evidence. Other CSI backends require their own
auditor; absence of an auditor is an error, never a cleanup pass. Unreachable
nodes, incomplete inventories and storage errors also fail verification.

## Bounded retries

`--attempts N` (default 3, at most 5) runs the same workload again after a
failure, never with lower counts. Each attempt is a fresh run (its own label
and namespaces) with its own evidence in `--out/attempt-N/`: `report.json`,
SQLite logs, `storage-<phase>.json` and any auditor stderr. `--out/summary.json`
lists every attempt with its failures, timings and whether its cleanup verified.

Every failure is classified, and only a **transient** one may be retried:

| Kind | Examples | Retried |
|---|---|---|
| transient | create failed (including a lost acknowledgement), partial startup (not every Pod finished by `--timeout`), a `sleep` Pod failed | yes, after `--retry-delay` seconds |
| integrity | SQLite evidence missing, invalid or unreadable; any `sqlite` Pod not Succeeded | no |
| storage | a backend audit failed or did not verify; not 100 distinct PVs | no |
| cleanup | residue after `--cleanup-timeout`, the `after` audit never verified, inventory errors | no |
| error | anything unexpected (API down, wrong reclaim policy, interrupt) | no |

A retry also requires that attempt's cleanup verified (API residue gone and,
for `sqlite`, the backend `after` audit). So a later success never follows a
leak or corruption, and a pass after retries is reported as such
(`retried`, `passed_on_attempt`) with the failed attempts beside it. The
runner exits 0 only when an attempt passed.

## Evidence and cleanup

Reports and logs survive cleanup in `--out`. If cleanup fails, the attempt's
`report.json` contains the remaining resources and run identity for recovery. A killed runner
cannot guarantee cleanup; preserve this directory and remove only its exact
namespaces after reviewing UIDs. No live results have been recorded yet.

## Selftest

`python3 tools/turbomode/selftest.py` (run it on dev: `sc-build 'python3
tools/turbomode/selftest.py -v'`) needs no cluster. Besides the SQLite and
pagination checks, it drives the real runner against an in-process fake API
with injected faults: lost create acknowledgement, watch 410, partial startup,
cleanup timeout (a PV that never goes), unreachable backend inventory,
unverified allocation, corruption and a failed SQLite Pod, and checks that each
keeps its evidence and that only transient, cleanly-cleaned failures retry.
The node auditor runs against a fake /proc, cgroup tree and stormblock API:
a clean node verifies (per-Pod directories recorded as unmeasured), a
leftover mount, process cgroup, cgroup directory or slab slot fails `after`,
99 of 100 volumes fails `allocated`, and a second cluster node, a missing
token, cgroup tree, hostPID or node name, an incomplete inventory or a
rejected token each make it exit non-zero.
