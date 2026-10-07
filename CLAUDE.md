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

`cargo test` runs qa-test's unit tests; qa-runner and must-gather have none. The test scripts under
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
| `crates/qa-test/src/` | the test container's `/test <suite>`: `turbomode.rs` load test (#26) + `turbo_audit.rs` node audit + `sqlite.rs` workloads + `turbomode_fake.rs` e2e tests; `short.rs` prerequisites; `medium.rs` namespace isolation (#18) + `agent.rs` serve/agent helper pods; `long.rs` overnight soak: kinds, sizing, trend, exit; container waves (#17) `containers.rs` + `claim.rs` workload; VM waves (#16) `wave.rs` steps, `census.rs` residue, `rdp.rs` X.224 probe; `ssh.rs`, `kube.rs`, `report.rs` shared; `crash.rs` SIGSEGV/SIGBUS report (`/test crash`) |
| `test/` | `build.sh` (static binary → `test/out/test`), `Containerfile` (scratch + `/test`), `requires.toml` (per-suite needs, stormcentral#55) |
| `tools/symbolize-crash.sh` | names a crash report's addresses (rebuilds the commit on the build box) |
| `tools/turbomode/` | the Python reference for `/test turbomode`: `run.py` (bounded retries), workload, stormblock auditor, `selftest.py` (fake API) |
| `STANDARD.md` | the test contract (metadata keys, env, exit codes) |
| `tests/<owner>/` | tests; owner defaults to `glennswest/<owner>` |
| `gather/<area>/` | must-gather collector scripts |

## Work plan

### Done — docs refresh from the code, changes since 2026-09-25 (2026-10-02)

- [x] README: turbomode in the intro/layout/anchors, 43 unit tests, status 2026-10-02, `turbomode/preflight` line, `STORM_TIMEOUT` per suite, `tools/turbomode/`
- [x] docs/presentation.md: turbomode slide row, helpers, interfaces, status, counts
- [x] STANDARD.md, main.rs/Cargo.toml descriptions: turbomode + sleep/sqlite helpers
- [x] CHANGELOG; sc-build on 17eb4f6: build + 43 tests pass (exit 0, 43 s). No new issues: no doc promise the code does not keep

### Done — #26 port turbomode to `/test turbomode` (owner: #33 option A) (2026-10-01 … 2026-10-03)

Owner (#33, 2026-09-30): driver, auditor and SQLite workload ported into the
one test image as `/test turbomode`; the Pod+PVC pairs go in the run
namespace (no extra namespaces). The sleeping profile gets the same
treatment (1,000 Pods in the run namespace), per the owner's reason "so no
extra namespaces are needed". `tools/turbomode/*.py` stay as the reference
and self-test oracle.

- [x] `turbomode.rs` driver: profiles sleep then sqlite, bounded attempts (1..5) with per-attempt evidence under `<results>/turbomode/<profile>/attempt-N/`, retry only transient failures with verified cleanup, watch-based latency, UID-precondition deletes by label, never force finalizers (93e5b9a..)
- [x] `turbo_audit.rs`: in-process stormblock/proc/mountinfo/cgroup auditor (before/allocated/after), one-node check
- [x] `/test sqlite` workload (rusqlite 0.37 bundled, sha2 0.11) and `/test sleep`; static musl binary smoke on dev: 1,000 records written/verified, a used claim refused
- [x] `turbomode_fake.rs`: driver e2e vs fake apiserver+stormblock (clean, lost claim ack → attempt 2, corruption final, leak final, no node → exit 2); 43 tests pass (sc-build 87baff9)
- [x] requires.toml `[turbomode]` cluster_read; README/CHANGELOG/Containerfile/tools README
- [x] Final sc-build on 0db1221: 24 Python self-tests, locked build, 43 tests, test/build.sh; progress on #26 (comment 5941244880); runner needs on stormcentral#247 (comment 5941244539)
- [x] 2026-10-02: stormcentral#247 closed: `[turbomode] budget_secs = 14400` (#39); attempts start only if their worst case fits the window (the Job deadline)
- [x] sc-build on bf7012f: 24 Python self-tests, locked build, 44 tests, test/build.sh (exit 0, 173 s); runner accepts `turbomode` (run 46418628a4 on C2NR0Q2); closed #39. That run built and pushed, then errored waiting for golden `test-stormcos_qa-…`: the registry sealed it as `test-stormcos-qa-…` (`_`→`-`), filed stormcentral#285 — blocks every stormcos_qa run
- [x] 2026-10-02: stormcentral#285 (golden name) closed; stormcentral#74 live (host access from requires.toml at `/host<path>`, `STORM_HOST_ROOT`). Host paths now default under `STORM_HOST_ROOT`; requires.toml drops the redundant `/proc/1/mountinfo`
- [x] sc-build on 4bb84d8: 24 Python self-tests, locked build, 45 tests, test/build.sh (exit 0, 218 s); progress on #26 (comment 5961373152); turbomode's cluster reads added to stormcentral#55 (comment 5961372932)
- [x] 2026-10-02 21:23Z: stormcentral#55 closed (cluster_read, 0744268/e8ecd68, golden d9ffbe91043c); closed #40 (fixed by 4bb84d8)
- [ ] Live run blocked (2026-10-02 22:00Z): (1) the VM still runs stormcentral f552113 (no cluster_read): the goldens API shows d9ffbe91043c `held` since 21:23Z behind `install server3 11.64` (power-cycle loop); (2) **rustkube-node#103**: every test container on C2NR0Q2 fails to start (`image /run/stormpump/images/clone-test-stormcos-qa-… was never pulled`, runs 2697e07bb2/82ff877efa "test container did not finish"). Proposed #26 `--after rustkube-node#103`. Next: once both clear, `stormcentral test run stormcos_qa turbomode` on C2NR0Q2; close #26 only on a passing run with latency + cleanup evidence
- [ ] 2026-10-02 22:35Z: rustkube-node#103 closed (fix 7d28509, golden d5ebc817f846), but **no release carries it**: 11.65's rustkube-node is d5c0d2c/46ee39f (both behind 7d28509), and C2NR0Q2 is still `last=11.56 failed`. Waits on stormcos#164 (the rustkube-node release request) and an install of that release on C2NR0Q2. Proposed #26 `--after stormcos#164`. Next unchanged: `stormcentral test run stormcos_qa turbomode` on C2NR0Q2 (check `short` reaches the pod log first)
- [ ] 2026-10-03: master: C2NR0Q2 out of use; **pvetest1 runs 11.72** (rustkube-node 31c042c9e59a, contains #103's fix). Running `stormcentral test run stormcos_qa short --tag pvetest1`, then `turbomode --tag pvetest1`
- [x] `short` run 4a3e735d59 on pvetest1: image built, pushed, Job ran, pod log reached (pass 2 fail 1 skip 1; the fail is a golden lookup 401, no stormblock token for `short`)
- [x] `turbomode` run 7f7d36b3c2 on pvetest1: **exit 139 (SIGSEGV) after 3,680 s**, no result line (the first is printed only when a profile ends), so the driver died at or just after the sleep profile's attempt 1 finish timeout (3,600 s). The pod log came back as one stripped stormpump warning: rustkube-node's `/log` strips the first 3 words of every non-CRI line with 3+ spaces, filed **rustkube-node#136**
- [x] Driver hardening (8085e02): fake e2e for Pods that never finish (timeout → transient → cleanup → retry); `main` boxes the suite future, tokio threads 16 MiB stacks; stderr progress lines. sc-build: Python self-tests, locked build, 46 tests, 13 turbomode tests under musl release, test/build.sh (exit 0, 199 s)
- [x] Re-run at 8085e02 (097feaf43d): exit 139 again at t+3,671 s. Progress lines: 1,000 creates in 2 s, **250/1000 finished by t+186 s then flat** for the rest of the hour (node allocatable pods 110), finish timeout, then the crash before `cleanup: deleting`. A 1,000-Pod/250-finish fake under musl release does not reproduce (6ff7b29)
- [x] 992c3e5: suites run on a 64 MiB `suite` thread (not main: unknown stack, overflow unreported on musl), more notes timeout→cleanup. sc-build: 47 tests, 14 turbomode under musl, image binary runs
- [x] The 250 plateau is the platform, not the test: a snapshot 4 min into run 3 showed every waiting Pod in ContainerCreating with Cilium IPAM `range is full`. Succeeded Pods keep their IPs, filed **rustkube-node#137**. All 1,000 Pods were bound to a 110-allocatable node, filed **rustkube#194**. The sleep profile cannot pass until #137 is fixed
- [x] Run 3 (4bbb76be8f, 992c3e5): exit 139 again on the suite thread, no overflow message; the last note was `report saved; cleanup`, so it died inside cleanup's first LIST of the 1,000 Pods. Pagination on rustkube checked by hand (works). The 1,000-Pod fake does not reproduce it
- [x] Crash report (91cf1fd..): SIGSEGV/SIGBUS handler prints addr/rip/rsp/frame-pointer chain/thread as `crash:` lines; reproducible test/build.sh (frame pointers, remapped paths); `tools/symbolize-crash.sh`. Verified on dev with `/test crash`
- [x] Run 4 (1fb84733cb, e8024b1): crash report came through: SIGSEGV addr 0x0 at `lock incq (%r12)`, r12=0, a null `Arc` clone in reqwest request building, after `report saved; cleanup` (i.e. in cleanup's first `list()`). The symbol is `turbo_audit::stormblock_items::{{closure}}+0x359` (callers: `Attempt::audit` only), but the frame chain goes straight to `attempt`. Points at heap corruption (safe code), only after a live hour-long attempt
- [ ] Debug on branch `debug/26-crash` (not for main): finish timeout 300 s + list notes, run **0483d48b1c**, queued behind 61d36acdb8 (an accidental repeat of run 4 at e8024b1; the runner has no cancel). If it crashes at ~300 s, iterate there.
- [x] pvetest1 reinstalled to 11.73 mid-way (61d36acdb8 and 0483d48b1c errored). Debug run b3e43d1fbb (300 s): all 3 sleep attempts cleaned up, then SIGSEGV with addr == rip inside our own text (`serde_json Value Display::fmt`). Debug run 61626abca7 (exe hash per note): **no crash**, whole suite ran, exit 1, `/test` byte-identical throughout. sqlite: 100 pairs, cleanup + after-audit verified in 15 s; integrity failed only because the evidence parser rejected stormpump's warning line
- [x] 5e42991: evidence skips non-object lines; image gets `/proc`, `/sys`; watch errors on stderr. sc-build: 49 tests, image builds and runs in podman
- [x] Debug run 1260fd7c36: **sqlite passed** (100 pairs, evidence + cleanup + after-audit, 12 s); the watch errors were the client's 60 s timeout ending watches rustkube keeps open past timeoutSeconds (rustkube#165, evidence added) → 6c965f9: a client-timeout end is a normal end (resume from rv), 50 tests
- [x] Full `main` run 7b8422bb2d (6c965f9): SIGSEGV again at ~1,940 s, 30 min into sleep attempt 1, now on a tokio worker: rip = first instruction of `drop_in_place<Observer::run closure>` (a task that never ends), addr 0. Pattern: every build without the per-minute exe hash crashed (19–60 min); both hashing builds (≤31 min) did not. Theory: evicted code pages of `/test` come back wrong from the image's backing (stormblock CoW clone), and hashing kept them hot
- [x] Debug run 7260943051 (907b16c, DONTNEED before each hash): **`/test`'s bytes changed on its volume** (same length, sha bb3b… → a8ac… → b3f9…) once the sqlite profile's 100 PVC volumes were being written; then SIGSEGV. Filed **stormblock#267**. Latency valid (watch fix works)
- [x] 2e…: turbomode re-reads its image (`--image-file`, cache dropped) after every attempt; a change is a final integrity failure. sc-build: 52 tests, image binary
- [ ] **Blocked**: stormblock#267 (image clone returns changed bytes; no turbomode run is trustworthy until fixed) and rustkube-node#137 (sleep profile: Succeeded Pods keep IPs). Proposed #26 `--after stormblock#267`. Next: once both ship to a test machine, `stormcentral test run stormcos_qa turbomode --tag pvetest1` at main; close on a pass (sleep + sqlite, latency valid, cleanup verified, image unchanged). Branch `debug/26-crash` is debug-only: delete it when #26 closes
- [ ] 2026-10-03 evening: stormblock#267 closed (fix 18ddf69, golden `golden-stormblock-668888675a35`, release request stormcos#168 open) and rustkube-node#137 closed (227fbfe). Release 11.77 has rustkube-node c248063 (contains 227fbfe) but stormblock 37bee74, **behind** 18ddf69; no test machine runs 11.77 (C2NR0Q2 install failed, pvetest1 11.73). No code change. Proposed #26 `--after stormcos#168`. Next unchanged: once a release with both fixes is installed on a test machine, `stormcentral test run stormcos_qa turbomode --tag <machine>` at main
- [ ] 2026-10-03 20:00Z: 11.78 is the first release with stormblock#267's fix (stormblock 507750d, 3 ahead of 18ddf69) and #137's, but it is **tombstoned**: on C2NR0Q2 the claim/VM probes timed out because stormblock's API stalled during flow-over (stormblock#269, fixed bc825e7, not in a release yet). pvetest1 still 11.73. Also #43: by day a test may take ≤30 min, so turbomode's 4 h budget only runs at night on a pve VM (pvetest1 qualifies) until #43 shrinks it. No code change. #26 stays `--after stormcos#168`. Next unchanged

- [ ] 2026-10-03 22:15Z: pvetest1 runs 11.78 (master: rerun now), but the runner refuses the 4 h `turbomode` by day (#325). Doing **#43** (owner: 100 Pods, ≤15 min): `turbomode` defaults 100 sleeping Pods / 25 SQLite pairs / 60 s sleep / finish 240 / cleanup 120 / reserve 120, `budget_secs = 900`; full scale moves to its own suite `turbomode-night` (budget 14400, night window). Then `stormcentral test run stormcos_qa turbomode --tag pvetest1` at main
- [x] #43 done (31b4b61, 5f8cf8b: `args_override_self` so a flag after the night preset wins). sc-build on 5f8cf8b: build, 53 tests, test/build.sh (exit 0, 130 s)
- [x] **Run 7277704177 on pvetest1 (11.78) at 5f8cf8b: passed, pass 2 fail 0.** sleep: 100 Pods, attempt 1, request→running p50 4.1 s / p95 61 s, cleanup verified 1.0 s, latency valid. sqlite: 25 Pod+PVC pairs, attempt 1, claim→running p50 8.0 s / p95 10.9 s, cleanup + after-audit verified 5.0 s, latency valid, image unchanged. Closed #26 and #43; deleted branch `debug/26-crash`. `turbomode-night` (budget 14400) declared, not yet run under the runner (night window only)

### In progress — #28 merge turbomode into main (2026-09-29)

- [x] `origin/main` was already an ancestor of `turbomode`; no new main changes or conflicts (verified 2026-09-29).
- [x] `sc-build 'python3 tools/turbomode/selftest.py -v && cargo build --locked && cargo test --locked'` on pushed head 9726c64: 18 self-tests passed, full workspace build passed, 27 QA suite tests passed (remote exit 0, 108s).
- [x] Merged as `aa880a6` (`Merge turbomode into main`); removed tracked Python bytecode and added ignore rules in `3d4e5a8`; both commits pushed to `main`.
- [x] `sc-build 'python3 tools/turbomode/selftest.py -v && cargo build --locked && cargo test --locked'` on pushed `main` head 3d4e5a8: 18 self-tests, full workspace build, 27 QA suite tests passed (remote exit 0, 104s).
- [x] Recorded evidence in `CHANGELOG.md`; closed #28 with the verified commits and test counts. No golden requested.

### In progress — #26 turbomode: bounded load-test retries + failure-path coverage (2026-09-29, branch `turbomode`)

Before #28, this work was isolated to the `turbomode` branch. #28 now directs
merging it into `main`; do not request goldens. Harness: `tools/turbomode/`
(`run.py`, `sqlite-workload.py`, `stormblock-audit.py`, `selftest.py`).

- [x] Merged origin/main into turbomode cleanly (f75662d); handoff step 2 `sc-build 'python3 tools/turbomode/selftest.py && cargo build --locked && cargo test --locked'` passes (no host-key failure from the VM)
- [x] Bounded retries (d02ac33): `--attempts N` (1..5), each attempt in `--out/attempt-N/` with its own report/logs/audits; retry only when every failure is transient (create error, partial startup, sleep-profile Pod failure) **and** that attempt's cleanup (+ backend after-audit) verified; integrity, storage, cleanup and unexpected failures are final; `summary.json` keeps every attempt
- [x] Failure-path selftests against an in-process fake API: lost create ack, watch 410, cleanup timeout, backend inventory failure, corruption, partial startup (d02ac33; fake backlog fix 096e7ba, #27)
- [x] README / CHANGELOG; `sc-build 'python3 tools/turbomode/selftest.py -v'` on 096e7ba: 18 tests OK
- [x] Owner selected C2NR0Q2 (Dell R230) on #26 / rustkube-node#110.
- [ ] Live validation blocked: `stormcentral testhost list` reports C2NR0Q2 `last=11.52 failed`; rustkube-node#110 says boot media and a release install are still needed. rustkube-node#102 tracks its live validation. Image digest and storage class are not recorded. Asked the owner for an approved non-root audit path on #26 (comment 5896707140); `stormcentral wait-owner` placed #26 in needs-owner. Do not start live workloads until answered and the target is restored.
- [x] Owner's audit-path decision (#26, 2026-09-29): the auditor runs **on the node as a test Job**, no ssh: runner-started in the run namespace, results in the pod log, read-only `hostPID` + hostPath `/proc`, `/sys/fs/cgroup`, `/proc/1/mountinfo` and only the file `/run/stormblock/engine/api_token`; namespace-scoped SA/Role; replace README's `root@` example.
- [x] Auditor collects locally from the host mounts (no ssh, no `TURBOMODE_NODES`), walks `/sys/fs/cgroup`, fails unless its node is the only cluster node; per-Pod kubelet dirs recorded `unmeasured` (not in the owner's mount list); 6 node-audit selftests; README Job spec; requires.toml `[turbomode]` (c347072). sc-build on 2f2b5b6 (after the #32 fix): 24 self-tests, build, 27 QA suite tests, exit 0. Closed #29 (decision) and #32 (build failure).
- [x] Commented stormcentral#74 with the exact Job needs (hostPID, read-only hostPaths incl. single files, downward spec.nodeName).
- [ ] **needs-owner** (#26 comment 5898356196, `wait-owner` done): how the driver and the audit Job fit — (A) port driver+auditor+SQLite workload to a `/test turbomode` suite in one Job (100 namespaces vs run namespace), (B) run.py from dev + audit-service Job, (C) other. Then live runs on C2NR0Q2 once it is install-ready and stormcentral#74 lands.
- [x] `sc-build 'python3 tools/turbomode/selftest.py -v && cargo build --locked && cargo test --locked'` on pushed `7f464dd`: 18 self-tests, full workspace build, 27 QA suite tests passed (remote exit 0, 86s).

### Done — merge PR #1 rustkube functional QA + topology ladder (2026-09-27)

- [x] Rebased onto main (STANDARD.md conflicts: kept main's corrected wording, added `QA-Topology`, `QA_MASTERS`, `QA_NODES`); README flags + topology gate, help text, CHANGELOG
- [x] main itself did not build (#21, `long::Args` private fields from bc37a0b): fixed in 8c74adb
- [x] sc-build passed on the branch (53519ab); merged with rebase (5d5171f)
- [x] sc-build on merged main passes (1d19e4b, 2026-09-27: build + 17 unit tests)

### Done — docs refresh from the code, changes since 2026-09-18 (2026-09-28)

- [x] README: status (the test container is what stormcentral runs; qa-runner has no caller), how the test image ships (`stormcentral test run` → `<machine>:5100/test-stormcos_qa-<suite>:<commit12>`, Job), shared env table with per-suite defaults, every suite's flags and defaults, `/test claim`, `wave-<k>/hold`, expected-fail list (rustkube-node#35 closed; vmcloud-image-operator#15 added), 27 unit tests (c073bca)
- [x] STANDARD.md: `QA-Topology` gates only (no cheapest-first, #3); rustkube tests' plain http (#11); qa-runner's contract vs the container's (c073bca)
- [x] test/Containerfile comment: long = both kinds, `claim` helper (c073bca)
- [x] docs/presentation.md: 14 slides, test container slide, counts from the headers (37 found, 32 on one node, 5 topology-gated, 7 in topology/single never run)
- [x] Filed #24 (medium drops node/LAN egress probes silently); added to #11 (rustkube tests hard-code http) and stormcentral#156 (config role "stale since 2026-08-09")
- [x] sc-build on fc698de: build + 27 unit tests pass; marp renders the deck to 14 slides

### Done — #17 overnight container waves (long) (2026-09-27 … 2026-10-05)

Owner (#17): container waves beside the VM waves: Deployments to the machine's
pod capacity, each pod with a stormblock claim, readiness and a Service; hold
(restart, reschedule, write + read the claim); drain (Deployments, pods,
claims, PVs, volumes all gone); repeat; per wave time to all-Ready, time to
drained, residue (memory, volumes, cgroups, veths, stale objects).

Design: `/test long` runs **both kinds, alternating** (test-standard: "every
night runs both kinds"); each kind preflights on its own, so a missing golden
stops only the VM waves (exit 2 only when nothing failed). Container wave =
N Deployments × 1 replica (a claim is RWO, so one pod per claim), default
StorageClass (the built-in `stormblock` driver), one Service per wave. N from
the node's free pod slots (allocatable − pods on it) × 0.8, min 10. Workload
`/test claim`: writes a token + 1 MiB blob to /data or verifies them, logs
`{"claim":"written|found|mismatch"}`, serves :8080, exits once (marker on the
claim) so the kubelet restarts it in place. Hold: Ready → Service endpoints +
ClusterIP → in-place restart with data kept → delete pod, replacement reads
the data. Slowdown on median create→Ready; residue adds veths, cgroups, own
Deployments/RS/pods/PVCs/Services/PVs/`pvc-<ns>-*` volumes.

- [x] `claim` helper mode (4427dc7)
- [x] `containers.rs` wave + census additions + `long` kinds/preflight/exit (4427dc7)
- [x] requires.toml (`pods`, `persistentvolumes` read) (4427dc7)
- [x] comment on stormcentral#55 (2026-09-27)
- [x] unit tests (claim, pod views, endpoints, claim log, schedule, cgroup slack); README / CHANGELOG
- [x] sc-build passes (0713ab1: build, 25 unit tests)
- [x] 0713ab1: `long` finds its own image under hostNetwork (`$HOSTNAME` is the node's there)
- [ ] live run on C2NR0Q2 (`--kinds containers`: VM waves blocked, #16); close #17 on a passing run. 2026-09-27: blocked — `stormcentral test run stormcos_qa short` (fd3f8a1fe0) built the image, but the push to C2NR0Q2's sbregistry got `500` on the chunked layer upload (stormblock-registry#56, commented), so no test image can reach the node. Next: once #56 is fixed, `stormcentral test run stormcos_qa short` (also checks `short` for real), then `long` through the runner once stormcentral#55 gives it `nodes`/`pods`/`persistentvolumes` read and hostNetwork — or by hand as a hostNetwork pod with `--image test-stormcos_qa-short:<commit12> --kinds containers --waves 1 --pods 10`
- 2026-09-28: stormblock-registry#56 fixed in v0.24.1 (closed 00:16Z). The runner queued `stormcos_qa short` run 66c3ad8a5e at 6a420cd, behind rustkube-node runs that are power-cycling C2NR0Q2. Waiting on it: does the image push work now, and does `short` pass
- 2026-09-28 17:17Z: 66c3ad8a5e still queued after ~1 h, behind rustkube-node 20d0bf509c (stuck at `power`) and 831e049d94 (stuck at `podman build`): stormcentral#139 (no step deadline; commented). C2NR0Q2 `last=11.52 failed`. Proposed #17 `--after stormcentral#139`. Next: when 66c3ad8a5e runs, check the push + `short`; then `sc-build 'cargo run -q -p stormcos-qa-test -- long --api https://192.168.30.2:6443 --insecure --namespace default --node 192.168.30.2 --kinds containers --waves 1 --pods 10 --image test-stormcos_qa-short:6a420cdbb4ec …'` (off-node residue sources report could-not-run)

- [ ] 2026-10-05: the runner can now run it, but `long` (8 h) is refused by day and on any machine that is not a pve VM (stormcentral#325), and C2NR0Q2 (11.80 passed) is bare metal. Plan, as #43 did for turbomode: a day-sized suite **`container-waves`** (`[container-waves] budget_secs = 900`): `/test container-waves` = `long --kinds containers --waves 3 --max-pods 20` (sizes 10, 20, 15: ramp, varying size, repeat); new `--max-pods` cap; stormblock's token also read from the host (`<STORM_HOST_ROOT>/run/stormblock/engine/api_token`, declared read-only — the runner never read `stormblock_token`). Then `stormcentral test run stormcos_qa container-waves --tag C2NR0Q2`; close #17 on a pass
- [x] ab62c10..b7be0fc: `container-waves` suite, `--max-pods`, host token path; sc-build on b7be0fc: build, 54 tests, test/build.sh (exit 0, 213 s). The first build caught `--kinds` appending on repeat (#50, fixed: `ArgAction::Set`)
- [x] Run c0fbef5f97 (b7be0fc): wave 1 Ready + Service, restart never seen, reason lost to rustkube-node#136. b03c25a: unspaced stderr copy of result lines, restart failures name pod state, 240 s steps. Run 44c6a4f24c: pods restarted (restarts 1, Ready) but `found` never parsed: `/test claim`'s own lines had spaces (#136). bb36656: claim lines unspaced; empty reschedule = skip. 55 tests
- [x] **Run 5d4c374633 on C2NR0Q2 (11.80) at bb36656: passed 20/0/0**, waves 10/20/15, residue flat. Closed #17. Night-scale `long` (both kinds, pve VM) not yet run under the runner

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
- [x] README / CHANGELOG / map: one-image section, `short` and `medium` sections; stale `vm-lifecycle` crate/YAML references gone (2026-09-27)
- [x] sc-build passes on main (1d19e4b, 17 unit tests incl. medium's 4)
- [x] test/build.sh + podman build on dev (df2eb70): 9.6 MB scratch image; `medium --help` runs; unreachable apiserver → JSON fail + exit 2; unknown mode → usage + exit 2. Found and fixed: `Cargo.lock` stale since the first commit, so `--locked` refused (df2eb70)
- Blockers for a live run (2026-09-27): stormcentral#55 (runner: `extra_namespaces`, cluster read, hostNetwork), stormvm#16 (VMs as pod-network endpoints), vmcloud-image-operator#15 (Fedora golden never placed on the node); also every runner run on C2NR0Q2 errors today (sbregistry :5100 refused, apiserver /readyz, stormcentral#63)
- [x] 2026-09-28 check: still blocked, nothing to change. stormcentral#55 (runner-made `iso` namespace, cluster read) has no runner-side answer. stormvm#16 has stormvm's half in main (8c91847, bridge binding + in-sandbox DHCP) but is still open and unreleased. vmcloud-image-operator#15's fix has not shipped (see #16). C2NR0Q2 is unreachable (no ping, :6443 refused)
- [ ] Close only after a run on a node: needs stormcentral#55 (namespaces create), stormvm#16 (pod-network VMs), stormcentral#63 (C2NR0Q2 apiserver)
- [ ] 2026-10-05 check: no code change. Cleared: stormcentral#55 (closed, but cluster_read only), pod-network VMs built (rustkube-node#88, rustkube#203 goldens; stormvm#16 open only for this live proof), vmcloud-image-operator#15 closed and stormcos#147 shipped in 11.53. **Still blocking:** stormcentral#183 (runner `extra_namespaces`: the `<run ns>-iso` namespace + Role binding + `STORM_NAMESPACE_ISO`; open, P2, no runner work yet) and stormcentral#376 (test-machine sbregistry 507 on push). Proposed #18 `--after stormcentral#183`. Next: when #183 lands, `stormcentral test run stormcos_qa medium --tag C2NR0Q2`; close #18 only on a pass

### In progress — #16 VM lifecycle soak (waves) (2026-09-25 …)

- [x] vm-waves 845557b202 (b9ec446) errored at the image build: runner still on dev (stormcentral b4ea803). Re-queue once the VM-build stormcentral installs (~09:40 CDT 2026-10-07)
- [x] Run 44bb10557c (f793a71): preflight 2..3 VMs, baseline, both VMs `Pending: waiting for golden fedora-44-x86_64` 600 s, drain clean. Auto-placement (vmcloud-image-operator#13) shipped in 11.88 but does not deliver: evidence on #13, proposed #16 `--after vmcloud-image-operator#13`. a0035b5: `long` preflight finds the golden by name in the volume list, so a missing golden stops VM waves at once (could not run) instead of 10 min per VM
- [ ] **Blocked** on vmcloud-image-operator#13. Next: when the golden reaches C2NR0Q2, `stormcentral test run stormcos_qa vm-waves --tag C2NR0Q2`; fix what ssh/RDP/install/restart show; close #16 on a pass

- [x] 2026-10-07: run 0ff84ae7bd (11.88, C2NR0Q2) pushed fine but skipped at preflight: MemAvailable holds 3 VMs, `--min-vms 5`. Fix b9ec446: `vm-waves` `--min-vms 2`; sc-build on a build VM: 58 tests, test/build.sh (exit 0, 145 s)
- [ ] `stormcentral test run stormcos_qa vm-waves --tag C2NR0Q2`, fix what the VM steps show

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
- [x] 2026-09-27 late (f81cb73, 7f00e4d): unreadable leftover sources → `residue/<source>` could-not-run; taps/veths only from the host netns; requires.toml `stormblock_token = true`, asked on stormcentral#55. sc-build: remote exit 0, 27 tests (the local wrapper then fails with `bpid: unbound variable`, stormcentral#122). Background: still blocked (placement Pending since 18:10Z; cause per vmcloud-image-operator#15: operator sends stormblock no token, #7/#12 there). Meanwhile, fix an honesty hole: under the runner (no stormblock token, no hostNetwork — stormcentral#55) the drain's "nothing left" passes with volumes/registrations unmeasured, and taps read from the pod's own netns count a wrong 0. Plan: taps/veths only from a host netns (bridge, `cilium_host` or `lxc_health` present), else unmeasured; an unmeasured leftover source the waves rely on → `residue/<source>` could-not-run (exit 2), never a pass; ask stormcentral#55 for the token
- [x] 2026-09-28 check: still blocked. vmcloud-image-operator#15's fix (74ed942) and stormcos#147 (token mount for vmimages) have not shipped: the latest stormcos release is 11.50 and #147 is still open. C2NR0Q2 is also unreachable from dev right now (no ping; :6443 and :9099 refused), so no live run was possible. Filed stormcentral#132 (sc-build: `mount point does not exist` after the reaper leaves a volume in use; it passed on retry)
- [ ] Re-run once vmcloud-image-operator#15 lands and the golden is on the node; close #16 only on a passing run
- [ ] (was blocked) first run against a real node. C2NR0Q2's apiserver refuses :6443 (2026-09-25), so nothing has been run end to end. When it is back: run the binary from dev with `--api https://192.168.30.2:6443 --insecure --token-file … --waves 1 --vms 2`, fix whatever the real API shows, then close #16
- Filed rustkube-node#65 (advertise KVM on the Node, so `requires: [kvm]` can be checked through the API) (expected to fail on a node until the stormvm/stormrdp/rustkube-node issues land)

- [ ] 2026-10-05: stormvm#40/#41/#22 and stormrdp#1 are still open, but their last notes are from 2026-09-27/28 (11.5x); C2NR0Q2 runs 11.80 and the runner works (#17 passed there). Plan, as for #17: a day suite **`vm-waves`** (`[vm-waves] budget_secs = 1800`, kvm, hostNetwork, stormblock token file): `/test vm-waves` = `long --kinds vms --waves 2 --min-vms 5 --max-vms 10 --ready-timeout 600 --install-timeout 300` (new `--max-vms`). Run it on C2NR0Q2 to see which step fails today; propose #16 `--after` whatever blocks it. Failed-start/recovery (owner's note from rustkube#104) is #31
- [x] 48ccfaf: `vm-waves` suite + `--max-vms`; sc-build: build, 55 tests, test/build.sh (exit 0, 200 s)
- [ ] **Blocked**: run 5cd836a908 (C2NR0Q2) could not push the image: sbregistry `507 Insufficient Storage` (stormcentral#376: per-commit test images never pruned; both pve VMs full too). Commented there with the census evidence (+6 volumes, +2 attachments per test image). Proposed #16 `--after stormcentral#376`. Next: when it closes, `stormcentral test run stormcos_qa vm-waves --tag C2NR0Q2` at main, then fix whatever the VM steps show (stormvm#40/#41/#22, stormrdp#1 may still be real)

- [ ] 2026-10-06: master: 11.88 (4 GiB node registry, stormcos#122: no more 507) passed every gate on C2NR0Q2; 11.88-flowsdn on pvetest1. Rerunning at main dd6ebdd: `vm-waves`, `short`, `container-waves` on C2NR0Q2; `turbomode` on pvetest1. `medium` stays blocked on stormcentral#183 (open)
- [x] Results at c4d32f3: `container-waves` 7d790122f8 passed 20/0; `short` 9af980ee2a failed `golden` 401 (#42, fixed in 2d2d7e6); `vm-waves` 0ff84ae7bd pushed fine (no 507) but **skipped at preflight**: MemAvailable allows 3 VMs, `--min-vms 5` (allocatable 15677 MiB × 0.8 / 2048 = 6). #16 next: day suite sized to what the machine holds (min 2–3), not 5; `turbomode` 254ae01320 errored: pvetest1's VM destroyed after its install (stormcentral#392, commented)

### Done — #34 short: smoke test, node-ready + system-pods (2026-10-07)

Design: `cluster_read` nodes, pods, namespaces in `[short]`. `node-ready`: every Node `Ready=True`, else name + conditions. `system-pods`: every pod in a namespace not labelled `storm.io/purpose=test` (the runner's run namespaces; a fresh test machine's other namespaces are the release's, kube-system included), minus pods labelled `storm.io/test-run`: Running with every container Ready, or Succeeded; restarts (containers + init) > `--max-restarts` (3) fail even when Running. Polled up to `--settle` (30 s; `short`'s budget is 120 s) until clean. A 403 on any of the three lists → `could not run`, exit 2 unless something really failed.

- [x] short.rs checks + 3 unit tests; requires.toml; README/CHANGELOG (e1ac68e)
- [x] 2026-10-07: dev.g8.lo retired; `SC_BUILD_VM=1 sc-build` on 7cc82cc (build VM): locked build, 58 tests, test/build.sh (exit 0, 132 s). Live `short` run 7f911d561b queued on C2NR0Q2 (covers #42 too)
- [x] Live short 7f911d561b (7cc82cc) errored: the runner still builds test images on dev (installed stormcentral b4ea803; the VM-build golden installs ~09:40 CDT 2026-10-07). Re-queue after it. (was) sc-build on e1ac68e **not run**: `dev.g8.lo: No route to host` (stormcentral#517, #521 P0). Proposed #34 `--after stormcentral#521`. Next: sc-build (fix any compile/test error), then live `short` on C2NR0Q2 (also checks #42's `golden`); close #34 on a run that shows both lines with a real answer
- [x] **Run fcc46c761d (C2NR0Q2, f793a71): `node-ready` pass (1 Node), `system-pods` pass (25 pods, kube-system)**; closed #34. `golden` no longer 401 (token accepted), closed #42; its 400 was stormblock#112 (GET by name), fixed a0035b5 (list + match). `helper-pod` skipped: own pod not found by $HOSTNAME, filed #54

### Done — #42 short: golden check 401 under the runner (2026-10-06)

- `census::stormblock_token()` already reads `$STORM_HOST_ROOT/run/stormblock/engine/api_token` (added for container-waves); `[long]` already declares the file. Left: `[short]` declares it; hint text names the host path
- [x] 2d2d7e6; sc-build: locked build, 55 tests, test/build.sh (exit 0, 261 s)
- [ ] Live `short` at ≥2d2d7e6 has not run: b13e57e579 interrupted (stormcentral restart), 1557ada79b no build slot in 60 min, ff76f7650a `dev.g8.lo: No route to host` (2026-10-07; stormcentral#517, #521 P0: builds still aimed at the retired dev). Proposed #42 `--after stormcentral#521`. Next: once test-image builds work again, `stormcentral test run stormcos_qa short --tag C2NR0Q2`; close #42 when `golden` is not a 401

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
