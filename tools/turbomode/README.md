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
TURBOMODE_NODES=/path/to/nodes.json python3 tools/turbomode/run.py sqlite \
  --api http://127.0.0.1:18001 --cluster TEST_MACHINE_RELEASE \
  --image PYTHON_IMAGE_WITH_SQLITE --storage-class stormblock \
  --storage-audit tools/turbomode/stormblock-audit.py --out /tmp/turbo-sqlite
```

Use the installed release's warm images and provisioned blank templates for a
warm run; identify cold runs separately. The Python image must provide Python 3
and its standard sqlite3 module. Pin image digests in recorded acceptance runs.
`nodes.json` maps **every** Kubernetes node (plus any separate storage node) to
SSH argv, e.g. `{"node-1": ["ssh", "-o", "BatchMode=yes", "root@test-host"]}`.
The audit needs Python 3 and root read access to all process mount/cgroup state.
It reads the engine token only on the node; no credentials enter reports.

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
