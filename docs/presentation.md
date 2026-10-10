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
Every claim here can be checked against crates/*/src/, test/, gather/,
README.md, or stormcentral's config/stormcentral.toml.
State as of 2026-10-07, version 0.1.0.
-->

# stormcos_qa

**The QA suite for stormcos clusters**

Test container · `must-gather`

v0.1.0 · 2026-10-07

---

## What it is and the problem it solves

A stormcos release is a whole node: kernel, ublk root, stormpump, fastetcd,
rustkube, stormblock and more. A regression can come from **any component**,
and it only shows up **on a booted node**.

- **The test container** (`/test <suite>`): the **system** tests stormcentral's
  test runner runs as a Job on the test machines, per its `docs/test-standard.md`
- **`must-gather`:** collects debug data through the API and a per-node
  collector pod (our `oc adm must-gather`)

Only system tests live here (burn, stress, soak, cross-component). A
component's own checks belong in its own test container (owner on rustkube#35).

---

## Where it sits in stormcos

From stormcentral's relationships graph (`config/stormcentral.toml`):

```
stormcos_qa  (group: qa)
   ├── depends_on ──▶ stormcos        the nodes under test
   └── depends_on ──▶ stormblock-csi  CSI collector
nothing depends on stormcos_qa
```

The suites drive the **rustkube** API, **stormvm** VMs, **stormrdp**,
**stormblock** volumes and claims, stormpump's boot report and **Cilium**
NetworkPolicy.

It is **not a stormcos component**: nothing from this repo is installed on a node.

---

## How it works

```
stormcentral test run stormcos_qa <suite> [--tag <machine>]
  └─ build box: test/build.sh + podman build ─▶ <machine>:5100/test-stormcos_qa-<suite>:<commit12>
       └─ Job in a run namespace: /test <suite> ─API─▶ rustkube, stormblock, stormvm
            └─ JSON lines + /results/*.jsonl, exit 0 pass / 1 fail / 2 could not run
```

Needs beyond a namespace-only Role (cluster reads, hostNetwork, hostPID,
read-only host files) are declared per suite in `test/requires.toml`; a check
without them reports **could not run**, never pass.

---

## The suites, one image

`FROM scratch` + `/test` and `/must-gather`; the same image is its own helper pods
(`serve`, `agent`, `claim`, `sleep`, `sqlite`) and must-gather's collector pods.

| Suite | What it checks |
|---|---|
| **short** | apiserver; every Node Ready, named, with a global IPv4; platform pods and stormpump's boot services up; root read-only on ublk; ssh answers; VMs served; the Fedora golden; a helper pod |
| **medium** (#18) | 5 VMs + 2 pods under `storm-isolate` reach each other and nothing else |
| **long** (#16, #17) | overnight waves of containers and VMs; every drain leaves nothing; slab space, engine memory and other residue may not grow |
| **container-waves**, **vm-waves** | `long`'s waves sized for a day run |
| **turbomode** (#26) | 100 Pods, then 25 Pods with SQLite claims, audited on the node; `turbomode-night` at full scale |
| **must-gather** (#45) | runs must-gather on the machine and checks its bundle |

---

## What it does today: must-gather

From a workstation or a pod, **no ssh** (#45):

| Part | How | What |
|---|---|---|
| cluster | the API, TLS + token | discovery, health, 22 resource lists, every CRD's objects; never Secrets/ConfigMaps |
| logs | `pods/log` | kube-system (the node's services are mirror pods) and pods in trouble |
| host, per node | a pinned pod: hostPID, hostNetwork, read-only mounts, `host-collect` | stormpump status + logs, stormd service logs, pod log files, `/dev/kmsg`, pstore, `/proc`, `/proc/net`, cgroups, stormblock engine, fastetcd metrics |
| collectors | `gather/<area>/*.sh` with `QA_API` + token | cadvisor, ironprom, stormblock-csi |

The host bundle returns through the pod log as checked base64 lines (no exec,
no node proxy). Keys and tokens are listed, never copied. Tarball +
`manifest.json` (inside it).

---

## Where the old scripts went (#30)

`tests/*.sh` and `qa-runner` had no caller since 2026-08-23 (#14). Retired:

| Was | Now |
|---|---|
| boot-to-multi-user, CRI-O, ublk root, ssh, hostname, node IP | `short`: `node-stack`, `root`, `ssh`, `node-identity` |
| rustkube (16), rustkube-node (5), stormblock-csi (3) | their own test containers; gaps filed there |
| fastetcd (3) | fastetcd's test container (#51) |
| ironprom (5) | not on a node yet; noted for its component (stormcentral#90) |
| image-has-gpt, SELinux | stormcos (an image check; a decision) |

---

## Interfaces

No ports of its own, no health endpoint, no metrics. Commands that run once and exit
(the `serve`/`claim` helpers answer TCP 8080 inside their own pods).

```
/test <suite> [--api https://<node>:6443 --insecure] [--node <ip>] …
      env: STORM_SUITE STORM_API STORM_NAMESPACE STORM_RUN_ID STORM_NODE
           STORM_TIMEOUT STORM_RESULTS STORM_HOST_ROOT
           STORMBLOCK_API_TOKEN / STORMBLOCK_TOKEN_FILE

must-gather --api https://<node>:6443 --token-file f [--ca-file ca | --insecure]
            [--host-image ref] [--out dir] [--nodes a,b] [--collectors-dir gather]
```

Configuration is flags and env only; there is no config file.

---

## How it ships and is operated

- **No golden and no stormcos component.** Nothing here is installed on a node
- **Built** with `sc-build` on a fresh build VM (`cargo build && cargo test`, Rust 2024)
- **Test container:** built, pushed to the test machine's registry and run as a Job by
  `stormcentral test run stormcos_qa <suite>`; results in `stormcentral test show <run>`
- **must-gather** for customers: a laptop binary and a golden (#48, waiting on the owner)
- **Updated** by pushing: the runner builds the image at the commit it runs

---

## Status

- **Passed on a node:** `turbomode` (pvetest1, 11.78), `container-waves` (C2NR0Q2, 11.80),
  `short`'s smoke test (C2NR0Q2, 11.88)
- VM waves wait on the Fedora golden reaching the node (vmcloud-image-operator#13)
- `medium` waits on the runner's extra namespace (stormcentral#183)
- must-gather and the new census numbers wait on a machine that makes Job pods
  (stormcentral#537) and on build VMs (stormcentral#535)

---

## Summary

- **The test container** is how stormcentral tests a machine: system suites,
  one scratch image, API-driven, exit 0/1/2
- **Component checks** live with their components; this repo keeps the system view
- **must-gather** collects without ssh, from a laptop or a pod

`README.md` · github.com/glennswest/stormcos_qa
