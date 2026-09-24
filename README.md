# stormcos_qa

The QA suite for stormcos images and clusters. It has four parts:

- **a test contract**, [STANDARD.md](STANDARD.md): a test is an executable
  script with `# QA-*:` metadata that passes when it exits 0;
- **tests**, under `tests/<owner>/`. Each component owns its own tests;
- **`qa-runner`**, which runs the tests against an image file and/or a live
  node. It files a GitHub issue in the owning repo for each failure (without
  duplicates), writes a JSON report, and exits with the number of blocking
  failures;
- **`must-gather`**, which collects debug data over SSH from one or more nodes.
  It runs built-in commands plus the collector scripts that components put in
  `gather/<area>/`.

Both binaries are command-line tools you run once and they exit. They have no
ports, no health endpoint and no metrics.

> **Status, 2026-09-24. Nothing runs this suite automatically at the moment.**
> The earlier docs said that "the builder" runs it after every build and
> tombstones (marks as failed) any image with a blocking failure. That was
> `stormcos-builder`, which was retired on 2026-08-23. No code in stormcos or
> stormcentral calls `qa-runner` today. `qa-runner` only *reports*: its exit
> code and the `tombstone` field in its report tell a caller what to do. It
> does not tombstone anything itself. Wiring up a new caller is
> [#14](https://github.com/glennswest/stormcos_qa/issues/14).

A 13-slide overview deck is at [docs/presentation.md](docs/presentation.md)
(Marp; render with `npx @marp-team/marp-cli docs/presentation.md`).

## Layout

```
STANDARD.md                 test contract: metadata, env, exit codes
docs/presentation.md        overview deck (Marp)
tests/<owner>/<test>        tests; owner defaults to glennswest/<owner>
tests/overall/<test>        cross-cutting tests; each must set QA-Owner
tests/topology/single/      single-node boot checks (NOT run yet, #8)
gather/<area>/<script>      must-gather collector scripts, owned by components
crates/qa-runner/           the runner
crates/must-gather/         the debug-data collector
```

The test directories today are `fastetcd`, `ironprom`, `overall`, `rustkube`,
`rustkube-node`, `stormblock`, `stormblock-csi`, `stormcos` and
`topology/single`. The collector directories are `fastetcd`, `ironprom`,
`kernel`, `stormblock` and `stormblock-csi`.

## Build

Workspace version `0.1.0`, Rust edition 2024. Build on the build box, never on
this VM and never as root:

```bash
git push && sc-build        # cargo build && cargo test on dev.g8.lo, scratch dir
```

There are no unit tests yet, so a successful compile is the only check.
`cargo test` does not run the test scripts, because they need a booted node.
The release profile uses `lto` and `strip`.

## How it ships

This repo produces **no golden** and is **not a stormcos component**. Nothing
from it goes onto a node. It is a stormcentral project in group `qa`, and
depends on `stormcos` and `stormblock-csi`. You run the binaries and scripts
from a checkout, on whatever machine can SSH to the node under test.

## qa-runner

```
qa-runner --release <id> [flags]
```

| Flag | Default | Effect |
|---|---|---|
| `--release <id>` | **required** | Release under test. Passed to tests as `QA_RELEASE_ID` and written into issues and the report. |
| `--tests-dir <dir>` | `tests` | Root of the test tree. |
| `--flavor <f>` | `""` | Passed to tests as `QA_FLAVOR`. |
| `--image <path>` | — | The built image file. Sets `QA_IMAGE` and **turns on `image`-scope tests**. |
| `--node-ip <ip>` | — | Sets `QA_NODE_IP`. It is also the node `--gather` collects from. |
| `--node-name <n>` | — | Sets `QA_NODE_NAME`. |
| `--ssh "<cmd>"` | — | SSH command prefix, e.g. `ssh -o StrictHostKeyChecking=no storm@<ip>`. Sets `QA_SSH` and **turns on `cluster`-scope tests**. |
| `--api <url>` | `http://127.0.0.1:6443` | Sets `QA_API`. rustkube serves TLS on 6443, so plain HTTP only works against an `--insecure` apiserver ([#11](https://github.com/glennswest/stormcos_qa/issues/11)). |
| `--artifacts <dir>` | `/tmp/qa-artifacts` | Created if missing. Holds each test's log (`<dir>-<name>.log`, lowercased, with non-alphanumerics turned into `-`). Tests get it as `QA_ARTIFACTS`. |
| `--file-issues` | off | File or update GitHub issues for failures. Uses the `gh` CLI, which must be logged in. |
| `--report <path>` | — | Write the JSON report here. |
| `--org <org>` | `glennswest` | Org used to build a test's default owner, `<org>/<top-dir>`. |
| `--gather` | off | If any test failed and `--node-ip` is set, run must-gather into `<artifacts>/must-gather-<release>`. |
| `--must-gather-bin <path>` | `must-gather` | must-gather binary that `--gather` runs. |
| `--collectors-dir <dir>` | — | Passed to must-gather as `--collectors-dir`, e.g. `gather`. |

What a run does, from `crates/qa-runner/src/main.rs`:

1. **Discover.** It looks at `<tests-dir>/<dir>/<file>`, **one level deep
   only**, so tests in deeper directories such as `topology/single/` are not
   found ([#8](https://github.com/glennswest/stormcos_qa/issues/8)). It skips
   dotfiles, `*.qa.toml` and `README.md`. A file must be executable to count.
   Metadata comes from `QA-<Key>: value` lines in the first 40 lines (see
   STANDARD.md). The run order is sorted by `QA-Name`.
2. **Scope-gate.** `image` tests run only with `--image`. `cluster` tests run
   only with `--ssh`. `component` tests always run. Skipped tests do not
   appear in the report.
3. **Resolve the owner.** An explicit `QA-Owner` wins. Otherwise the owner is
   `<org>/<top-dir>`. An `overall/` test without `QA-Owner` is skipped with a
   message on stderr and is not counted.
4. **Run** each test one at a time, with `QA-Timeout` (300 s by default). When
   the timeout is hit the test is killed and counts as failed. stdout and
   stderr both go to the log file.
5. **File issues** (with `--file-issues`). The issue goes in the owner repo. If
   an open issue already has the test's marker, the runner adds a comment with
   the last 40 log lines instead. Otherwise it creates `QA failure: <name>`
   with up to 6000 characters of log. A test that passes again does **not**
   close its issue ([#10](https://github.com/glennswest/stormcos_qa/issues/10)).
6. **Gather** (with `--gather`). This runs must-gather **without** passing on
   `--ssh`, so it connects as `root@<node>`
   ([#12](https://github.com/glennswest/stormcos_qa/issues/12)).
7. **Report and exit.** It prints `[PASS]`/`[FAIL] <name> (<owner>)` per test,
   then a summary line that ends in `— TOMBSTONE` if there was a blocking
   failure. The exit code is `min(blocking failures, 125)`, so **0 means
   nothing blocking failed**.

The report (`--report`) looks like this:

```json
{ "release": "…", "flavor": "…", "total": 0, "passed": 0, "failed": 0,
  "blocking_failures": 0, "tombstone": false,
  "results": [ { "name": "…", "owner": "glennswest/…", "scope": "cluster",
                 "blocking": true, "passed": false, "duration_secs": 3,
                 "log": "first 4000 chars", "issue": "url or owner#n (updated)" } ] }
```

## must-gather

```
must-gather --nodes <ip>[,<ip>…] [--out /tmp/mg] [--collectors-dir gather]
```

| Flag | Default | Effect |
|---|---|---|
| `--nodes <a,b>` | **required** | Nodes to collect from, separated by commas. |
| `--ssh "<template>"` | `ssh -o StrictHostKeyChecking=no -o ConnectTimeout=8 root@{node}` | `{node}` is replaced by the node. The template is split on whitespace, and the remote command is appended as one argument. |
| `--out <dir>` | `/tmp/must-gather` | Output directory. The tarball `<out>.tar.gz` is written next to it. |
| `--collectors-dir <dir>` | — | Extra collector scripts, `<dir>/<area>/<executable>`. |
| `--timeout <s>` | `60` | Timeout for each command or script. |

For each node, the output goes to `<out>/<node>/<area>/<name>.txt`. If a
command writes to stderr, that output is appended after a `--- stderr ---`
line. A timeout writes `(collector timed out)` instead of failing.

**Built-in collectors** (run on the node over SSH, **with no `sudo`**):

| Area | Names |
|---|---|
| `kernel` | `uname`, `cmdline`, `dmesg` (last 800 lines), `modules`, `io_uring_disabled`, `taint` |
| `system` | `os-release` (+ `/etc/stormcos-release`), `systemd-failed`, `systemd-running`, `boot-warnings`, `resources` |
| `storage` | `block` (lsblk, `/dev/ublk*`), `mounts` |
| `network` | `addr`, `route`, `listen` |
| `cluster` | `nodes`, `pods`, `events`, fetched with `wget http://127.0.0.1:6443/api/v1/…` (does not work against TLS, [#11](https://github.com/glennswest/stormcos_qa/issues/11)) |
| `components` | `journalctl -u` + `systemctl status` for `kubelet`, `kube-proxy`, `kube-apiserver`, `kube-controller-manager`, `kube-scheduler`, `fastetcd`, `crio`, `cadvisor`, `ironprom`, `stormblock`, `stormblock-target`, `sshd`, `NetworkManager` |

The `system` and `components` collectors assume a **systemd** node, which is
the 2026-08-19 image. stormcos's new boot chain runs stormpump as PID 1 with no
systemd ([#14](https://github.com/glennswest/stormcos_qa/issues/14)).

**Component collectors** are scripts in `gather/<area>/`. They run **locally**,
once per node, with `QA_NODE_IP=<node>` and `QA_SSH=<expanded ssh template>`
set. `QA_API` is **not** set, so scripts fall back to their own default. Each
script reaches the node with `$QA_SSH "…"`. The current collectors are:

| Script | Collects |
|---|---|
| `gather/fastetcd/status.sh` | unit status, `GET /health` on `$FASTETCD_ENDPOINT` (default `http://127.0.0.1:2379`), data dir and backups, `fastetcd fsck`, journal |
| `gather/ironprom/status.sh` | pods in ns `monitoring`, then on `<podIP>:9090`: buildinfo, `status/tsdb`, `status/runtimeinfo`, targets, rules, `ironprom_*` self-metrics |
| `gather/kernel/ublk-io_uring.sh` | `io_uring_disabled`, `/dev/ublk*`, `ublk_drv`, ublk sysfs/debugfs |
| `gather/stormblock/status.sh` | `stormblock` and `stormblock-target` unit status, `/etc/stormblock/meta/`, root mount |
| `gather/stormblock-csi/status.sh` | pods in `stormblock-system`, `wanderingvolumes` and `volumepolicies` (`stormblock.io/v1alpha1`), `stormblock-tiebreak` and node leases, `csistoragecapacities`, nvme, `/dev/ublkb*` |

When it finishes, must-gather tars `<out>` into `<out>.tar.gz` (the `tar` exit
status is ignored) and then writes `<out>/manifest.json`. The manifest has
`nodes`, `collectors`, `out` and `tarball`. Because it is written after the
tar, the manifest is **not inside** the tarball
([#13](https://github.com/glennswest/stormcos_qa/issues/13)).

## Adding tests and collectors

Put executables in `tests/<your-repo>/` and follow [STANDARD.md](STANDARD.md).
Put collector scripts in `gather/<area>/`. See the existing ones for the
`$QA_SSH` pattern.
