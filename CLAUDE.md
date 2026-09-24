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

There are no unit tests yet; the compile is the check. The test scripts under
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
| `STANDARD.md` | the test contract (metadata keys, env, exit codes) |
| `tests/<owner>/` | tests; owner defaults to `glennswest/<owner>` |
| `gather/<area>/` | must-gather collector scripts |

## Work plan

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
- #7 docs: a presentation of its purpose and functionality (after #6)
- #5 test all three boot modes
- #3 topology-scoped tests
- #2 qa-runner: provision multi-master + full topologies
