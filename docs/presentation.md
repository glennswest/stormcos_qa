---
marp: true
theme: default
paginate: true
title: stormcos_qa
description: Purpose and functionality of the stormcos QA suite
---

<!--
Render: npx @marp-team/marp-cli docs/presentation.md          (HTML)
        npx @marp-team/marp-cli --pdf docs/presentation.md    (PDF)
Every claim here can be checked against crates/*/src/main.rs, tests/, gather/,
README.md, STANDARD.md, or stormcentral's config/stormcentral.toml.
State as of 2026-09-24, version 0.1.0.
-->

# stormcos_qa

**The QA suite for stormcos images and clusters**

Tests · `qa-runner` · `must-gather`

v0.1.0 · 2026-09-24

---

## What it is and the problem it solves

A stormcos release is a whole node: kernel, ublk root, CRI-O, kubelet,
fastetcd, rustkube, stormblock and more. A regression can come from **any
component**, and it only shows up **on a booted node**.

stormcos_qa gives each component a way to check its own behaviour on a
real image, and it sends every failure **back to the repo that owns it**:

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

Its tests and collectors also exercise **fastetcd, rustkube, rustkube-node
and stormblock** (all stormcentral projects), plus **ironprom**, which is a
GitHub repo but not a stormcentral project.

It is **not a stormcos component**: nothing from this repo runs on a node.

---

## How it works

```
 tests/<owner>/*  ──discover──▶ qa-runner ──QA_* env──▶ test ──$QA_SSH──▶ node
   # QA-* meta      (1 level)      │                     exit 0 = pass
                                   ├─ fail + --file-issues ─▶ gh issue in <owner>
                                   │    (dedup by <!-- qa:… --> marker)
                                   ├─ fail + --gather ─▶ must-gather ─ssh─▶ node
                                   │                     + gather/<area>/*
                                   │                     ─▶ <out>/<node>/<area>/*.txt
                                   │                        + <out>.tar.gz
                                   └─▶ report.json + exit = min(blocking fails,125)
                                                      │
                                              caller tombstones
                                          (no caller today, #14)
```

---

## The test contract (STANDARD.md)

- A test is **any executable** in `tests/<owner>/`, and **exit 0 means pass**
- Metadata comes from `QA-<Key>: value` lines in the first 40 lines (`#` or `//`)

| Key | Default |
|---|---|
| `QA-Name` | filename without `.sh` |
| `QA-Owner` | `glennswest/<dir>`, and it is **required** in `overall/` |
| `QA-Scope` | `cluster` (also `image` or `component`) |
| `QA-Severity` | `blocking` (only `warn` is non-blocking) |
| `QA-Timeout` | `300` s |

The environment a test gets: `QA_RELEASE_ID`, `QA_FLAVOR`, `QA_API` and
`QA_ARTIFACTS` are always set. `QA_IMAGE` is set when `--image` is given, and
`QA_NODE_IP`, `QA_NODE_NAME` and `QA_SSH` when the node flags are given.

---

## What it does today: qa-runner

- **Discovers** executables in `tests/<dir>/` and runs them sorted by name
- **Scope-gates:** `image` tests run only with `--image`, `cluster` tests only with `--ssh`, and `component` tests always
- **Runs** one test at a time with a timeout (kill = fail) and logs to `<artifacts>/<dir>-<name>.log`
- **Files issues** with `--file-issues`, via `gh`:
  - opens `QA failure: <name>` in the owner repo
  - or, if an issue with the same marker is still open, comments on it
- **Gathers** after a failure with `--gather`, by running must-gather against `--node-ip`
- **Reports** as JSON with `tombstone: true` when there is a blocking failure; the exit code is the number of blocking failures

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

## What it does today: the tests

**22 tests are found and run: 16 blocking (15 cluster + 1 image) and 6 warn.**

| Owner dir | Tests |
|---|---|
| fastetcd | health, put/get round-trip, 1000 namespaces |
| ironprom | pod ready, `/-/healthy` and `/-/ready`, API surface, PromQL, self-metrics (warn) |
| rustkube | apiserver `/healthz` + node list |
| rustkube-node | node Ready, has IP, local DNS · gateway and outbound (warn) |
| stormblock | ublk root is erofs |
| stormblock-csi | driver pods, operator leader, PVC provisions (all warn) |
| stormcos | image has GPT (image) · boots to multi-user · QE key present |
| overall | ssh reachable (owner stormcos) |

`topology/single/` holds **7 more tests that are never run** (#8).

---

## Interfaces

**CLI only.** There are no ports, no health endpoint and no metrics. Both
tools are commands you run once and they exit.

```
qa-runner --release <id> [--tests-dir tests] [--flavor f]
          [--image <file>] [--node-ip ip --node-name n --ssh "<cmd>"]
          [--api http://127.0.0.1:6443] [--artifacts /tmp/qa-artifacts]
          [--file-issues] [--report out.json] [--org glennswest]
          [--gather --must-gather-bin must-gather --collectors-dir gather]

must-gather --nodes a[,b] [--ssh "ssh … root@{node}"] [--out /tmp/must-gather]
            [--collectors-dir gather] [--timeout 60]
```

The outputs are the JSON report, the exit code, GitHub issues, and
`<out>.tar.gz`. The only configuration is flags; there is no config file.

---

## How it ships and is operated

- **No golden and no stormcos component.** Nothing here is installed on a node
- **Built** with `sc-build` on dev.g8.lo (`cargo build && cargo test`, Rust 2024)
- **Run** from a checkout, on any machine that can SSH to the node under test
- **Updated** by pushing. The next run of a checkout picks up new tests, and a component adds its own tests or collectors with a commit here
- **Issue filing** needs a logged-in `gh`
- **Node lifecycle is the caller's job:** the runner neither provisions nor tears down the test node (#2)

---

## Status: what does not work yet

- **Nothing runs the suite automatically.** stormcos-builder, which did, was retired on 2026-08-23. The new build path is not chosen yet (#14, **needs an owner decision**)
- Tests in nested directories (`topology/single/`) are never discovered (#8)
- `QA_API` and the cluster collectors use `http://` on 6443, but rustkube serves TLS there (#11)
- `--gather` connects as `root@`, not the test's `storm` SSH user (#12)
- The systemd-based collectors and tests do not fit stormpump-PID-1 nodes (#14)
- Smaller gaps: `.qa.toml` is never read (#9), there is no auto-close on pass (#10), and the manifest is not in the tarball (#13)

---

## Planned (not built)

- **A caller for the QA pass** that provisions a throwaway node, runs `qa-runner`, and tombstones on a non-zero exit (#14, #2)
- **Topologies:** single, 3-master, and full 3 masters + 3 nodes, each with its own tests (#3, #2)
- **All three boot modes:** qcow2 run-in-place, ISO move-to-disk, iSCSI network root (#5)
- **Close the contract gaps** above: #8–#13

---

## Summary

- **One contract:** executable + `QA-*` metadata + exit code, owned by each component
- **One runner:** scope-gated, issues filed without duplicates in the owner's repo, and the exit code is the tombstone verdict
- **One gatherer:** built-ins plus component collectors, tarred for each run
- **The gap that matters:** no build runs it today (#14)

`README.md` · `STANDARD.md` · github.com/glennswest/stormcos_qa
