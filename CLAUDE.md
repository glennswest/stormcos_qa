# CLAUDE.md — stormcos_qa

The stormcos QA suite: the system test container (`/test <suite>`, run by
stormcentral's test runner on booted test machines), its boot-log review
(`bootlog/`), and the live test of `must-gather`, which moved with its
collector scripts to glennswest/stormgather (#48). The old `tests/*.sh` scripts and `qa-runner` were retired in
#30; component checks belong in each component's own test container.

Read the cross-project rules in `../CLAUDE.md` first — in particular **build
with `sc-build` after pushing, never on this VM, never as root**.

## Build and test

```bash
git push
sc-build                       # cargo build && cargo test, on dev.g8.lo
```

`cargo test` runs qa-test's unit tests (must-gather's run in stormgather). The suites
themselves need a booted node (`stormcentral test run stormcos_qa <suite>`).

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
| `crates/qa-test/src/` | the test container's `/test <suite>`: `turbomode.rs` load test (#26) + `turbo_audit.rs` node audit + `sqlite.rs` workloads + `turbomode_fake.rs` e2e tests; `short.rs` prerequisites (incl. `ssh-keys` #66, `sc-login`, `boot-source`, `engine-audit` #79); `topology.rs` `three-master`/`full-six` (#3); `failstart.rs` failed-start case (#31); `podapi.rs` in-pod Kubernetes API parity (#82), driver + `/test podapi-probe`; `medium.rs` namespace isolation (#18) + `agent.rs` serve/agent helper pods; `long.rs` overnight soak: kinds, sizing, trend, exit; container waves (#17) `containers.rs` + `claim.rs` workload; VM waves (#16) `wave.rs` steps, `census.rs` residue, `rdp.rs` X.224 probe; `ssh.rs`, `kube.rs`, `report.rs` shared; `engine.rs` the stormblock engine's client, http or https with the node CA (#81); `crash.rs` SIGSEGV/SIGBUS report (`/test crash`); `mustgather.rs` (`/test must-gather`, #45) |
| glennswest/stormgather | must-gather, `gather/` and the retry helper (moved #48): `qa-test` takes `stormgather-retry` by git at a pinned rev; `test/build.sh` installs must-gather at that rev. Call-site table: `docs/retries.md` |
| `test/` | `build.sh` (static binary → `test/out/test`; must-gather from stormgather → `test/out/must-gather`), `Containerfile` (scratch + `/test`), `requires.toml` (per-suite needs, stormcentral#55) |
| `tools/symbolize-crash.sh` | names a crash report's addresses (rebuilds the commit on the build box) |
| `tools/turbomode/` | the Python reference for `/test turbomode`: `run.py` (bounded retries), workload, stormblock auditor, `selftest.py` (fake API) |
| `bootlog/` | boot-log review (#67, stormcos#545): `known.toml` known console lines (pattern → open issue → owner), `check.py` unexplained lines per console capture, `test_check.py` |

## Work plan and history

Moved to [docs/work-plan.md](docs/work-plan.md) (CLAUDE.md is loaded into every session; its size made the session's context thrash). Nothing was dropped. Not loaded by default: `grep -n '^##' docs/work-plan.md`, then read only the section you need. Write new intentions and progress there, not here.

