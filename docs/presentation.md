---
marp: true
theme: default
paginate: true
title: stormcos_qa
description: Purpose and functionality of the stormcos QA suite
---

<!--
Render: npx @marp-team/marp-cli docs/presentation.md          (HTML)
        npx @marp-team/marp-cli --pdf docs/presentation.md    (PDF; needs Chrome/Edge/Firefox)
When stdin is not a terminal (ssh, CI), add </dev/null, or marp reads stdin as a second input.
Every claim here can be checked against crates/*/src/, test/, tests/, gather/,
README.md, STANDARD.md, or stormcentral's config/stormcentral.toml.
State as of 2026-10-02, version 0.1.0.
-->

# stormcos_qa

**The QA suite for stormcos images and clusters**

Test container · tests · `qa-runner` · `must-gather`

v0.1.0 · 2026-10-02

---

## What it is and the problem it solves

A stormcos release is a whole node: kernel, ublk root, CRI-O, kubelet,
fastetcd, rustkube, stormblock and more. A regression can come from **any
component**, and it only shows up **on a booted node**.

- **The test container** (`/test short|medium|long`, and the `turbomode` load
  test): the suites stormcentral's test runner runs as a Job on the test
  machines, per its `docs/test-standard.md`
- **tests:** plain executables that pass by exiting 0, owned per component
- **`qa-runner`:** runs them, files one issue per failure in the owner's repo, and returns a verdict on whether the release should be tombstoned
- **`must-gather`:** collects debug data from the nodes (our `oc adm must-gather`)

---

## Where it sits in stormcos

From stormcentral's relationships graph (`config/stormcentral.toml`):

```
stormcos_qa  (group: qa)
   ├── depends_on ──▶ stormcos        the images and nodes under test
   └── depends_on ──▶ stormblock-csi  CSI tests + collector
nothing depends on stormcos_qa
```

The suites drive the **rustkube** API, **stormvm** VMs, **stormrdp**,
**stormblock** volumes and claims, and **Cilium** NetworkPolicy. The scripts
also exercise fastetcd, rustkube-node and **ironprom** (not a stormcentral
project yet, stormcentral#90).

It is **not a stormcos component**: nothing from this repo is installed on a node.

---

## How it works: two paths

```
stormcentral test run stormcos_qa <suite>
  └─ build box: test/build.sh + podman build ─▶ <machine>:5100/test-stormcos_qa-<suite>:<commit12>
       └─ Job in a run namespace: /test <suite> ─API─▶ rustkube, stormblock, stormvm
            └─ JSON lines + /results/*.jsonl, exit 0 pass / 1 fail / 2 could not run

tests/<owner>/* ──▶ qa-runner ──QA_* env──▶ test ──$QA_SSH──▶ node
  # QA-* meta          ├─ fail + --file-issues ─▶ gh issue in <owner> (deduped)
  (1 level deep)       ├─ fail + --gather ─▶ must-gather ─ssh─▶ node ─▶ <out>.tar.gz
                       └─▶ report.json + exit = min(blocking fails, 125)
                           (no caller today, #14)
```

---

## The test container: three suites + a load test, one image

`FROM scratch` + one static binary `/test`; the same image is its own helper pods
(`serve`, `agent`, `claim`, `sleep`, `sqlite`), so a run fetches nothing from outside the machine.

| Suite | What it checks |
|---|---|
| **short** | apiserver answers, VirtualMachines served, the Fedora golden on the node's stormblock, a helper pod comes up |
| **medium** (#18) | 5 VMs + 2 pods in a namespace under `storm-isolate` (stormconsole's isolate policy) reach each other and nothing else; a control pass first |
| **long** (#16, #17) | overnight **waves**, alternating kinds, sized from the machine: containers (N Deployments × 1 pod, each with a built-in `stormblock` claim) and VMs (ssh, RDP, install, restart, package kept); every drain must leave nothing, and a slower wave or growing residue fails |
| **turbomode** (#26, explicit) | 1,000 sleeping Pods, then 100 Pods each with its own claim writing and re-checking 1,000 SQLite records; a read-only audit on the node proves the volumes allocated, then gone; bounded retries only for clean transient failures |

Needs beyond a namespace-only Role are declared in `test/requires.toml`
(stormcentral#55, #74); a check without them reports **could not run**, never pass.

---

## The test contract (STANDARD.md, for qa-runner)

- A test is **any executable** in `tests/<owner>/`, and **exit 0 means pass**
- Metadata comes from `QA-<Key>: value` lines in the first 40 lines (`#` or `//`)

| Key | Default |
|---|---|
| `QA-Name` | filename without `.sh` |
| `QA-Owner` | `glennswest/<dir>`, and it is **required** in `overall/` |
| `QA-Scope` | `cluster` (also `image` or `component`) |
| `QA-Topology` | `single` (also `multi-node`, `full`: skipped unless the run has the nodes) |
| `QA-Severity` | `blocking` (only `warn` is non-blocking) |
| `QA-Timeout` | `300` s |

Always set: `QA_RELEASE_ID`, `QA_FLAVOR`, `QA_API`, `QA_ARTIFACTS`. With flags:
`QA_IMAGE`, `QA_NODE_IP`, `QA_NODE_NAME`, `QA_SSH`, `QA_MASTERS`, `QA_NODES`.

---

## What it does today: qa-runner

- **Discovers** executables in `tests/<dir>/` and runs them sorted by name
- **Scope-gates:** `image` tests run only with `--image`, `cluster` tests only with `--ssh`, and `component` tests always
- **Topology-gates:** `multi-node` needs masters + nodes ≥ 3, `full` ≥ 3 of each; otherwise `[SKIP]`
- **Runs** one test at a time with a timeout (kill = fail) and logs to `<artifacts>/<dir>-<name>.log`
- **Files issues** with `--file-issues`, via `gh`: a new `QA failure: <name>`, or a comment on the open one with the same marker
- **Gathers** after a failure with `--gather`, by running must-gather against `--node-ip`
- **Reports** as JSON with `tombstone: true` on a blocking failure; the exit code is the number of blocking failures

---

## What it does today: must-gather

For each node, over SSH (default `root@{node}`, 60 s per command):

| Area | Built-ins |
|---|---|
| kernel | uname, cmdline, dmesg, lsmod, io_uring_disabled, taint |
| system | os-release, failed and running units, boot warnings, resources |
| storage / network | lsblk + `/dev/ublk*`, mounts / addr, route, listening sockets |
| cluster | nodes, pods, events from `:6443` |
| components | journal + status for 13 units (kubelet … stormblock, sshd) |

It also runs the **component collectors** in `gather/`: fastetcd, ironprom,
kernel (ublk/io_uring), stormblock and stormblock-csi. The result is a tarball
plus `manifest.json`.

---

## What it does today: the test scripts

**37 tests are found: 32 run on a single node** (26 blocking incl. 1 image, 6 warn);
**5 need more nodes** (3 multi-node, 2 full) and are skipped.

| Owner dir | Tests |
|---|---|
| fastetcd | health, put/get round-trip, 1000 namespaces |
| ironprom | pod ready, `/-/healthy` and `/-/ready`, API surface, PromQL, self-metrics (warn) |
| rustkube | 16: healthz, CRDs, SSA, patches, selectors, watch bookmarks, events · all nodes Ready, scheduling, DaemonSet (multi-node) · HA read-your-write, leader election (full) |
| rustkube-node | node Ready, has IP, local DNS · gateway and outbound (warn) |
| stormblock | ublk root is erofs |
| stormblock-csi | driver pods, operator leader, PVC provisions (all warn) |
| stormcos | image has GPT (image) · boots to multi-user · QE key present |
| overall | ssh reachable (owner stormcos) |

`topology/single/` holds **7 more tests that are never run** (#8).

---

## Interfaces

No ports of its own, no health endpoint, no metrics. Commands that run once and exit
(the `serve`/`claim` helpers answer TCP 8080 inside their own pods).

```
/test short|medium|long|turbomode [--api https://<node>:6443 --insecure] [--node <ip>] …
      env: STORM_SUITE STORM_API STORM_NAMESPACE STORM_RUN_ID STORM_NODE
           STORM_TIMEOUT STORM_RESULTS STORMBLOCK_API_TOKEN / STORMBLOCK_TOKEN_FILE
      turbomode: TURBOMODE_NODE _PROC _HOST_MOUNTINFO _CGROUP _TOKEN _STORMBLOCK _KUBELET_PODS

qa-runner --release <id> [--image f] [--node-ip ip --ssh "<cmd>"]
          [--masters a,b --nodes c,d] [--api http://127.0.0.1:6443]
          [--file-issues] [--report out.json] [--gather …]

must-gather --nodes a[,b] [--ssh "ssh … root@{node}"] [--out /tmp/must-gather]
            [--collectors-dir gather] [--timeout 60]
```

Configuration is flags and env only; there is no config file.

---

## How it ships and is operated

- **No golden and no stormcos component.** Nothing here is installed on a node
- **Built** with `sc-build` on dev.g8.lo (`cargo build && cargo test`, Rust 2024, 43 tests incl. 5 turbomode end-to-end against a fake apiserver)
- **Test container:** built, pushed to the test machine's registry and run as a Job by
  `stormcentral test run stormcos_qa <suite>`; results in `stormcentral test show <run>`
- **Scripts, qa-runner, must-gather:** run from a checkout, on any machine that can
  SSH to the node; issue filing needs a logged-in `gh`
- **Updated** by pushing: the runner builds the image at the commit it runs
- `tools/turbomode/` (Python) is turbomode's reference and selftest oracle, run by hand

---

## Status: what does not work yet

- **No suite has passed on a node yet.** The image push failed (stormblock-registry#56, fixed), runs queue behind hung ones (stormcentral#139)
- The runner cannot start `turbomode` (stormcentral#247) or give its Job hostPID and read-only hostPaths (stormcentral#74)
- VM suites wait on the Fedora golden (vmcloud-image-operator#15), pod-network VMs (stormvm#16), `accessCredentials`, restart and RDP (stormvm#41, #22, stormrdp#1)
- Under the runner, `long` lacks cluster read, `hostNetwork` and stormblock's token, and `medium` its extra namespace (stormcentral#55)
- **Nothing runs `qa-runner`** since stormcos-builder retired (#14, **owner decision**)
- Script gaps: nested dirs never run (#8), `http://` vs TLS on 6443 (#11), `--gather` as `root@` (#12), no stormpump equivalents of the systemd checks (#14), `.qa.toml` (#9), auto-close (#10), manifest outside the tarball (#13)

---

## Planned (not built)

- **Live runs** of `short`, `medium` and nightly `long` on every test machine, closing #16–#18; `turbomode` on C2NR0Q2, closing #26
- **Checks not covered yet:** isolation ingress from the node and the LAN (#23), skip lines for dropped node/LAN egress probes (#24)
- **A home for the scripts:** a caller for `qa-runner`, or the scripts moved into the test container (#14)
- **Topologies and boot modes:** single, 3-master and 3 + 3 (#3, #2); qcow2, ISO move-to-disk, iSCSI root (#5)

---

## Summary

- **The test container** is how stormcentral tests a machine: three suites and a load test, one scratch image, API-driven, exit 0/1/2
- **One script contract:** executable + `QA-*` metadata + exit code, owned by each component
- **One runner, one gatherer** for the scripts, waiting on a caller (#14)
- **The gap that matters:** nothing has passed on a node yet

`README.md` · `STANDARD.md` · github.com/glennswest/stormcos_qa
