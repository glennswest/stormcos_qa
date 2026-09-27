# CLAUDE.md — stormcos_qa

The stormcos QA suite: a test contract (`STANDARD.md`), per-component test
scripts (`tests/`), a runner that executes them and files GitHub issues on
failure (`qa-runner`), and a debug-data collector (`must-gather`) with
component-owned collector scripts (`gather/`).

Read the cross-project rules in `../CLAUDE.md` first — in particular **build
with `sc-build` after pushing, never on this VM, never as root**.

## Build and test

```bash
git push
sc-build                       # cargo build && cargo test, on dev.g8.lo
```

`cargo test` runs vm-lifecycle's unit tests; qa-runner and must-gather have none. The test scripts under
`tests/` are not run by `cargo test` — they need a booted node (see README).

## Version locations

| File | Field |
|---|---|
| `Cargo.toml` | `[workspace.package] version` (both crates inherit it) |
| `CHANGELOG.md` | latest release heading |
| git tag | `vX.Y.Z` |

Current version: **0.1.0** (no tag cut yet).

## Shipping

stormcos_qa is **not a stormcos component** and produces **no golden**: nothing
here is on a node. It is a stormcentral project (group `qa`) only. There is
therefore no `stormcentral component build` step for this repo.

## Map

| Path | Job |
|---|---|
| `crates/qa-runner/src/main.rs` | discover `tests/<dir>/<file>`, scope-gate, run with `QA_*` env, file/dedupe issues, JSON report, exit = blocking failures |
| `crates/must-gather/src/main.rs` | built-in remote collectors over SSH + `gather/<area>/*` scripts, per node, tarball + manifest |
| `crates/vm-lifecycle/src/` | the VM soak (#16): `wave.rs` steps, `census.rs` residue, `rdp.rs` X.224 probe, `ssh.rs` in-process login, `main.rs` sizing/trend/exit |
| `test/` | `Containerfile` → `stormcos_qa-test-vm-lifecycle`; `vm-lifecycle.yaml` Job + RBAC (stormcentral test-standard) |
| `STANDARD.md` | the test contract (metadata keys, env, exit codes) |
| `tests/<owner>/` | tests; owner defaults to `glennswest/<owner>` |
| `gather/<area>/` | must-gather collector scripts |

## Work plan

### Done — merge PR #1 rustkube functional QA + topology ladder (2026-09-27)

- [x] Rebased onto main (STANDARD.md conflicts: kept main's corrected wording, added `QA-Topology`, `QA_MASTERS`, `QA_NODES`); README flags + topology gate, help text, CHANGELOG
- [x] main itself did not build (#21, `long::Args` private fields from bc37a0b): fixed in 8c74adb
- [x] sc-build passed on the branch (53519ab); merged with rebase (5d5171f)
- [x] sc-build on merged main passes (1d19e4b, 2026-09-27: build + 17 unit tests)

### In progress — #18 namespace isolation test (medium) (2026-09-27)

Owner (#18, and stormvm#16 comments): 5 VMs + 2 pods in a namespace isolated
exactly as stormconsole's "isolated namespace" action does it (NetworkPolicy
`storm-isolate`: same-namespace in/out only, Cilium-enforced; VMs covered only
as pod-network endpoints, stormvm#16). Test standard changed since #16: one
image, `/test <suite>`, `test/build.sh`, and stormcentral's runner gives a
namespace-only Role (no cluster reads, no hostNetwork, no namespace create).

Design: the driver (the Job, in the run namespace) stays **outside**; it
creates a second namespace `<run ns>-iso` (run-labelled; the runner already
deletes run-labelled namespaces) with the policy, 5 VMs, 2 server pods, and
short-lived in-namespace **agent** pods (`/test agent`) that ssh into the VMs
and probe the matrix, reporting through their pod log. An outside server pod
sits in the run namespace. A control pass before the policy decides which
outside targets are meaningful (a target unreachable even without the policy
is a skip, not a pass).

- [x] Restructure: one binary, `/test short|medium|long` (+ `serve`, `agent`), `test/build.sh`, Containerfile COPY — #16's soak becomes `long` (bc37a0b)
- [x] `medium`: the isolation test (bc37a0b, `crates/qa-test/src/medium.rs` + `agent.rs`)
- [x] `short`: prerequisites (apiserver, VM CRD, golden) (bc37a0b)
- [x] `test/requires.toml` (cluster needs, per the proposal in stormcentral#55); commented there 2026-09-27
- [ ] README / CHANGELOG / map: README still describes `crates/vm-lifecycle` and the deleted `test/vm-lifecycle.yaml`; no changelog entry for the restructure, `short` or `medium` — in progress 2026-09-27
- [x] sc-build passes on main (1d19e4b, 17 unit tests incl. medium's 4)
- [ ] test/build.sh + podman build on dev
- [ ] Close only after a run on a node: needs stormcentral#55 (namespaces create), stormvm#16 (pod-network VMs), stormcentral#63 (C2NR0Q2 apiserver)

### Blocked — #16 VM lifecycle soak (waves) (2026-09-25)

Per the owner's comments on #16: a standing `long`-suite test on every test
machine (mixed hardware), packaged per stormcentral `docs/test-standard.md`
as the container `stormcos_qa-test-vm-lifecycle`, run as a Job; everything
discovered through the API; `requires: [kvm]` → skip where absent. First
overnight **wave** scenario: ramp VMs (10 … ~80% allocatable memory), hold
(ssh+RDP up, install a package, restart, package kept), drain (nothing
left), repeat through the window; per-wave start latency + residue trend.

- [x] Research the stormvm / rustkube / stormrdp / stormblock APIs the steps use
- [x] `crates/vm-lifecycle`: the soak binary (JSON-lines output, exit 0/1/2)
- [x] `test/Containerfile` + Job manifest for `stormcos_qa-test-vm-lifecycle`
- [x] README / CHANGELOG (STANDARD.md is qa-runner's script contract; the container follows stormcentral's)
- [x] sc-build passes (cc7a993: build + 10 unit tests)
- [x] container builds on dev (podman, scratch + static binary, `--help` runs); unreachable apiserver → JSON fail line + exit 2 (d2fcdb2)
- [x] 2026-09-27 first live run (C2NR0Q2 11.50, from dev via sc-build, `--waves 1 --vms 2`): preflight, baseline, create, drain and the residue check all work against the real API. Both VMs stayed `Starting`, VMI status null, for 15 min
- [x] Cause 1 (test): the soak pins with `spec.nodeName`; the scheduler skips it, so no `status.nodeName`, and rustkube-node's VMI list/watch is `fieldSelector=status.nodeName=` → the kubelet never sees the VMI. A probe with `nodeSelector: kubernetes.io/hostname` was scheduled and picked up at once
- [x] Cause 2 (environment): the kubelet then reports `waiting for golden fedora-44-x86_64` — the golden is gone from C2NR0Q2 since the 11.50 reinstall. Automatic Fedora goldens: vmcloud-image-operator#11/#13, stormcos#129
- [x] Fix: pin via `nodeSelector` (scheduler path) (ff1c16a)
- [x] Fix #19: send stormblock's token (census + preflight + short); a 401 is not "golden present" (639b1f3)
- [x] Filed rustkube-node#85: VMI list/watch by `status.nodeName` only drops VMIs placed by `spec.nodeName`
- [x] sc-build on 639b1f3 — covered by the merged-main build (1d19e4b)
- [x] 2026-09-27 re-run (C2NR0Q2 now 11.51, node `storm-06f96d`, run b54a9227): ff1c16a verified — both VMIs got `status.nodeName` and the kubelet picked them up within a minute (`waiting for golden fedora-44-x86_64`), vs status null for 15 min before. The run was cut off by a local `timeout 1800` (26 min of it waiting for a build slot); its 2 VMs + ssh secret were deleted by hand. Command: `sc-build 'cargo run -q -p stormcos-qa-test -- long --api https://192.168.30.2:6443 --insecure --namespace default --node 192.168.30.2 --waves 1 --vms 2 --timeout 1500 --results "$PWD/results"; …; true'` — give the local timeout ≥ 1 h
- [x] Golden: vmcloud-image-operator now runs on the node; CloudImage `fedora-44` is `Available` (18:10Z) but its CloudImagePlacement stays `Pending: waiting for the fleet golden: fedora-44 is Building` — vmcloud-image-operator#15 (P0; commented with today's evidence). **#16 is blocked on it**
- Still unverified from dev: stormblock volume residue (no stormblock token off-node; the in-cluster Job reads it from the host)
- [ ] Re-run once vmcloud-image-operator#15 lands and the golden is on the node; close #16 only on a passing run
- [ ] (was blocked) first run against a real node. C2NR0Q2's apiserver refuses :6443 (2026-09-25), so nothing has been run end to end. When it is back: run the binary from dev with `--api https://192.168.30.2:6443 --insecure --token-file … --waves 1 --vms 2`, fix whatever the real API shows, then close #16
- Filed rustkube-node#65 (advertise KVM on the Node, so `requires: [kvm]` can be checked through the API) (expected to fail on a node until the stormvm/stormrdp/rustkube-node issues land)

### Done — #7 docs: a presentation of its purpose and functionality (2026-09-24)

- [x] `docs/presentation.md`: Marp deck, 8–15 slides, every claim from the code / README (#6) / stormcentral config
- [x] README links the deck; CHANGELOG entry
- [x] sc-build passes (5b1e20c) and the deck renders to 13 HTML slides with marp-cli on dev; close #7

### Done — #6 docs: update the documentation from the code (2026-09-24)

- [x] Add this CLAUDE.md and CHANGELOG.md (neither existed)
- [x] README.md rewritten from the code: every flag + default, env, exit codes, report schema, what runs it today (nothing — stormcos-builder retired 2026-08-23), how it ships (no golden)
- [x] STANDARD.md corrected: mark `.qa.toml`, auto-close, nested test dirs, tombstoning as not implemented
- [x] Crate doc comments: drop "builder tombstones" as present-tense fact
- [x] File issues for doc promises the code does not keep: #8 nested test dirs not run, #9 `.qa.toml` not read, #10 no auto-close, #11 http vs TLS apiserver, #12 `--gather` ssh as root, #13 manifest not in tarball, #14 nothing runs the QA pass / systemd assumptions (needs owner decision)
- [x] sc-build passes (13b8e0e, exit 0); close #6

### Open issues

- #8–#13 docs-vs-code gaps found in #6 (code fixes)
- #14 no caller runs qa-runner since stormcos-builder retired — **needs owner decision**
- #5 test all three boot modes
- #3 topology-scoped tests
- #2 qa-runner: provision multi-master + full topologies
