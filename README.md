# stormcos_qa

The QA suite for stormcos images and clusters. It has four parts:

- **a test contract**, [STANDARD.md](STANDARD.md): a test is an executable
  script with `# QA-*:` metadata that passes when it exits 0;
- **tests**, under `tests/<owner>/`. Each component owns its own tests;
- **`qa-runner`**, which runs the tests against an image file and/or a live
  node. It files a GitHub issue in the owning repo for each failure (without
  duplicates), writes a JSON report, and exits with the number of blocking
  failures;
- **`vm-lifecycle`**, the VM lifecycle soak (#16): a test **container**
  (`stormcos_qa-test-vm-lifecycle`, `long` suite) that stormcentral runs as a
  Job, per its `docs/test-standard.md`. See [below](#vm-lifecycle-the-vm-soak-in-waves);
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
(Marp; `npx @marp-team/marp-cli docs/presentation.md` renders HTML; `--pdf`
also needs Chrome, Edge or Firefox on the machine, and dev.g8.lo has none).

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
crates/vm-lifecycle/        the VM lifecycle soak (a test container, #16)
test/Containerfile          builds stormcos_qa-test-vm-lifecycle (scratch, static)
test/vm-lifecycle.yaml      its Job, ServiceAccount and RBAC, as the runner applies them
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

`cargo test` runs `vm-lifecycle`'s unit tests (RDP packet encoding, tap
names, quantities, wave sizing, the residue rule). qa-runner and must-gather
have none. `cargo test` does not run the test scripts or the soak, because
they need a booted node.
The release profile uses `lto` and `strip`.

## How it ships

This repo produces **no golden** and is **not a stormcos component**. Nothing
from it is installed on a node; the `vm-lifecycle` test container runs on a
test machine as a Job and is deleted with its namespace. It is a stormcentral project in group `qa`, and
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

## vm-lifecycle: the VM soak in waves

The owner's ask (#16): create 10 VMs, check their ssh and RDP ports, install
a package, restart them and check the package is still there, delete them,
and repeat. It runs as the first **overnight wave** scenario of stormcentral's
test standard: a standing `long`-suite test on **every** test machine, mixed
hardware, sized from each machine's own capacity.

```
podman build -f test/Containerfile -t stormcos_qa-test-vm-lifecycle .
```

The image is `FROM scratch` with one static binary, `/test`. It does ssh
(russh, with a per-run ed25519 key) and the RDP probe in-process.
[`test/vm-lifecycle.yaml`](test/vm-lifecycle.yaml) is the Job the runner
applies in the run's namespace. It declares `suite: long` and
`requires: [kvm]`, and uses `hostNetwork` (see below).

**A wave** (every VM in a wave runs concurrently):

1. **Ramp.** Create N `VirtualMachine`s (`kubevirt.io/v1`) in
   `STORM_NAMESPACE`, labelled `storm.io/test-run=<STORM_RUN_ID>`. Each
   clones its root disk from `--golden` (`fedora-44-x86_64`), gets the run's
   key through **`accessCredentials`** (a Secret, `noCloud` propagation), is
   bridged onto `--bridge` (`storm.io/bridge: stormbr0`), has a graphics
   device (for RDP), and is pinned to the node under test.
2. **Up.** The VMI is `Running` with an address in `status.interfaces[]`.
   **ssh** on port 22 accepts the run's key and runs a command. **RDP**
   through stormrdp on `<node>:3389` accepts an X.224 Connection Request with
   routing token `vm/<ns>/<name>` and returns `RDP_NEG_RSP`. The probe stops
   before TLS and login. `create → ssh login` is the VM's **start latency**.
3. **Install** `--package` (`jq`) with `dnf` or `apt-get` over ssh, and
   record its version. The guests fetch it from their distro mirrors; that is
   the only thing the suite fetches from outside the cluster.
4. **Restart.** `PUT …/virtualmachines/<vm>/restart`. Wait for a VMI with a
   new uid to be up (step 2). Then the package must still be installed at the
   same version, which proves the root disk survived.
5. **Drain.** Delete every VM. Within `--drain-timeout`, none of the run may
   be left: no VMs or VMIs, no stormblock volume named `<ns>.<vm>-*` or
   `<vm>-seed`, no stormvm registration, no tap `vm%08x` (stormvm's name,
   FNV-1a of `<ns>/<vm>/default`).

**Waves.** The first wave is the smallest (`--min-vms`, 10), because it is the
latency baseline. The largest is `--capacity-fraction` (0.8) × the Node's
`status.allocatable.memory` ÷ `--vm-memory-mib` (2048). It is also capped by
the host's `MemAvailable`, less 1 GiB, at guest memory plus 10%. Later waves
vary across that range. Waves repeat until `STORM_TIMEOUT` (less a drain
reserve) would be overrun, or `--waves N` (alias `--cycles`). `--vms N` fixes
every wave at N. If the machine cannot hold the smallest wave, the test
reports **skip**, not pass.

**Residue and slowdown.** A census is taken before the first wave and after
every drain:

| metric | source |
|---|---|
| stormblock volumes / attachments | `<node>:9090/api/v1/volumes`, `…/{id}/attach` |
| stormvm registrations | `127.0.0.1:9095/api/v1/vms` (loopback-only on stormcos, hence `hostNetwork`) |
| taps | the host's `/proc/net/dev` (no API lists them) |
| node memory in use | `/proc/meminfo` (`MemTotal − MemAvailable`) |
| allocated file handles | `/proc/sys/fs/file-nr` |

A wave **fails** in any of these cases:

- a VM failed any step;
- anything of the run is left after the drain;
- a metric is above the baseline by more than its slack **and** above the
  previous drain, so it is still growing and not a one-off plateau. The slack
  is 0 for counts, `--mem-slack-mib` 512 and `--fd-slack` 2048;
- its median start latency is more than `--slowdown` (1.5) × wave 1's, plus
  `--slowdown-grace-secs` (30).

A source that cannot be read is reported as unmeasured (`null`), never as 0.

**Output**, per test-standard.md:

- one JSON object per line on stdout, also appended to
  `/results/vm-lifecycle.jsonl`. There is a line for each step,
  `wave-<k>/<vm>/{create,up,rdp,install,restart}`, a `wave-<k>/drain` line,
  and a `wave-<k>` line whose `wave` field holds the wave's record. The last
  line is `{"summary":…}`;
- the trend in `/results/waves.json`;
- a failing VM's VM, VMI and events in `/results/wave-<k>-<vm>.json`.

The exit code is 0 when everything passed or was skipped, 1 when something
failed, and 2 when the test could not run. It cannot run when the apiserver
is unreachable or refuses, the VirtualMachine resource is not served, the
node under test cannot be identified, or the golden is not in the node's
stormblock.

**Environment:** `STORM_API` (empty means in-cluster), `STORM_NAMESPACE`,
`STORM_RUN_ID`, `STORM_NODE` (the node's address or name; stormblock and
RDP are reached at it), `STORM_TIMEOUT` (8 h if unset) and `STORM_RESULTS`
(`/results`). Run `vm-lifecycle --help` for every flag. Outside a cluster,
use `--api https://<node>:6443 --token-file <f> --insecure`.

`--seed-key` also puts the key in the cloud-init user-data. It is a
diagnostic bypass while stormvm#41 is open, so that the later steps can be
measured. It is **not** a pass of #16.

**Expected to fail until** these are fixed:

- stormvm#40 (a bridged VMI reports no address);
- stormvm#41 (`accessCredentials` is not read);
- stormrdp#1, stormcos#69 (stormrdp is not on the node yet);
- stormvm#22 (a restart re-clones the root disk, so the package is lost);
- rustkube-node#35 (delete stops the VM).

Passing 10 × 10 is the definition of done for that set.

**Not yet:**

- the first failure's console log. It is only reachable as a WebSocket
  (`…/virtualmachineinstances/<vm>/console`), and the soak does not read it;
- `requires: [kvm]` is declared, but no Node advertises KVM through the API
  yet, so only the runner can match it;
- nothing runs the container yet. stormcentral's scheduling of `long`
  suites is its own work plan item.

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
