# Changelog

## [Unreleased]

### 2026-09-27
- **fix:** `/test long` and `/test short` send stormblock's bearer token (from `STORMBLOCK_API_TOKEN`, `STORMBLOCK_TOKEN_FILE`, `/etc/stormblock/api_token`, `/var/lib/stormblock/api_token`) to its volume API. `long`'s preflight no longer reads a 401 as "golden present"; `short` says stormblock refused instead of "not on the node" (#19)
- **fix:** `/test long` pins its VMs with a `kubernetes.io/hostname` nodeSelector instead of `spec.nodeName`: a hand-placed VMI never gets `status.nodeName`, which is all rustkube-node's kubelet watches, so no VM of the soak ever started (found in the first live run on C2NR0Q2) (#16)
- **fix:** qa-test: `long::Args` fields are `pub(crate)` so `wave.rs` can read them; the one-image restructure did not compile (#21)
- **feat:** rustkube functional QA tests (`tests/rustkube/`), one behaviour per file, run from the QA process against `http://$QA_NODE_IP:6443`: events group + translation, node-conditions strategic merge, server-side apply, DeleteOptions, WatchList bookmark, PartialObjectMetadata, json-patch test-null CAS, CRD lifecycle, label/field selectors, nodes Ready, Deployment pod Running, DaemonSet one-per-node, HA read-your-write, leader-election lease (PR #1)
- **feat:** qa-runner `QA-Topology` ladder (`single` / `multi-node` / `full`) with `--masters` / `--nodes` → `QA_MASTERS` / `QA_NODES`; a test needing a bigger topology than the run has is skipped, not failed (PR #1)
- **docs:** STANDARD.md documents the ladder and that cluster tests run in the QA process, never on node userland; README lists the new flags and the topology gate (PR #1)

### 2026-09-25
- **feat:** `vm-lifecycle`, the VM lifecycle soak as overnight waves (#16). It ramps VMs from 10 up to about 80% of allocatable memory. Each VM must be up (Running with an address, ssh accepting the run's key through `accessCredentials`, and RDP through stormrdp), install a package, restart and keep the package. Then it drains the wave and checks nothing is left. It repeats through `STORM_TIMEOUT`. Across waves it measures start latency and residue (stormblock volumes and attachments, stormvm registrations, taps, node memory, file handles), and fails a wave that is slower than the first or leaves growing residue. Output is JSON lines, exit 0/1/2
- **feat:** `test/Containerfile` (image `stormcos_qa-test-vm-lifecycle`, scratch with a static binary) and `test/vm-lifecycle.yaml` (Job, ServiceAccount, RBAC; `long`, `requires: [kvm]`, `hostNetwork`), per stormcentral docs/test-standard.md (#16)
- **docs:** README section on vm-lifecycle: steps, wave sizing, residue rule, output, env, what it is expected to fail on until the stormvm/stormrdp/rustkube-node issues land (#16)

### 2026-09-24
- **docs:** `docs/presentation.md`, a 13-slide Marp deck on purpose, place in stormcos, how it works, what works today, interfaces, shipping, status and planned work; linked from README (#7)
- **docs:** note that the deck's PDF export needs a browser and that marp needs `</dev/null` when stdin is not a terminal (#7)
- **docs:** qa-runner and must-gather module docs match behaviour (no self-tombstoning, one-level discovery, manifest outside tarball, collector env) (#6)
- **docs:** STANDARD.md corrected against qa-runner: one-level discovery, 40-line metadata scan, severity/scope/timeout fallbacks, real issue marker format, env set unconditionally vs per flag; `.qa.toml`, auto-close, nested test dirs and tombstoning marked not implemented (#8, #9, #10, #14) (#6)
- **docs:** README rewritten from the code: every qa-runner and must-gather flag with its default, discovery, scope gating, issue filing, report schema, exit code, built-in and component collectors; states that nothing runs the suite since stormcos-builder was retired and that it ships no golden (#6)
- **docs:** Add `CLAUDE.md` (work plan, version locations, shipping) and this changelog (#6)
- **feat:** stormblock-csi cluster tests + must-gather collector
- **fix:** root-fs checks were false-negative on a correct image
- **feat:** assert `authorized_keys` is storm-owned/readable
- **feat:** network reachability tests + QE-key-present guard
- **feat:** `tests/topology/single/` boot checks (#3)
- **feat:** ironprom and fastetcd tests + must-gather collectors
- **feat:** test standard, qa-runner, must-gather, example tests
