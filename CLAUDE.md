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

### In progress — #16 VM lifecycle soak (waves) (2026-09-25)

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
- [ ] container builds on dev; end-to-end run blocked: C2NR0Q2's apiserver refuses :6443 (2026-09-25)
- [ ] close #16 (expected to fail on a node until the stormvm/stormrdp/rustkube-node issues land)

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
