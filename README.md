# stormcos_qa

The QA suite for stormcos images and clusters. It has four parts:

- **a test contract**, [STANDARD.md](STANDARD.md): a test is an executable
  script with `# QA-*:` metadata that passes when it exits 0;
- **tests**, under `tests/<owner>/`. Each component owns its own tests;
- **`qa-runner`**, which runs the tests against an image file and/or a live
  node. It files a GitHub issue in the owning repo for each failure (without
  duplicates), writes a JSON report, and exits with the number of blocking
  failures;
- **the test container** (`crates/qa-test`, `test/`): one image that
  stormcentral runs as a Job per its `docs/test-standard.md`, started as
  `/test short|medium|long`. `short` checks what the VM suites stand on,
  `medium` is namespace isolation (#18), `long` is the overnight soak in
  waves of containers (#17) and VMs (#16). See [below](#the-test-container-test-shortmediumlong);
- **`must-gather`**, which collects debug data over SSH from one or more nodes.
  It runs built-in commands plus the collector scripts that components put in
  `gather/<area>/`.

`qa-runner`, `must-gather` and the test binary are command-line tools you run
once and they exit. None of them has a port of its own, a health endpoint or
metrics. The test binary's helper modes (`serve`, `claim`) listen on TCP
8080 inside their own pods, as a probe target.

> **Status, 2026-09-28.** The **test container** is what stormcentral runs:
> `stormcentral test run stormcos_qa <suite>` builds the image and runs it as a
> Job on a test machine (see [How it ships](#how-it-ships)). No run has reached
> a node yet: the image push failed on stormblock-registry#56, and runs now
> queue behind stormcentral#139. **Nothing runs `qa-runner` or the `tests/`
> scripts.** The earlier docs said that "the builder" runs it after every build and
> tombstones (marks as failed) any image with a blocking failure. That was
> `stormcos-builder`, which was retired on 2026-08-23. No code in stormcos or
> stormcentral calls `qa-runner` today. `qa-runner` only *reports*: its exit
> code and the `tombstone` field in its report tell a caller what to do. It
> does not tombstone anything itself. Wiring up a new caller is
> [#14](https://github.com/glennswest/stormcos_qa/issues/14), an owner
> decision: give it a caller, or move the scripts into the test container.

A 14-slide overview deck is at [docs/presentation.md](docs/presentation.md)
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
crates/qa-test/             the test container's binary: /test short|medium|long|serve|agent|claim
test/build.sh               builds that binary static (musl) into test/out/test
test/Containerfile          the test image (scratch + /test)
test/requires.toml          what each suite needs beyond a namespace-only Role (stormcentral#55)
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

`cargo test` runs `qa-test`'s 27 unit tests: RDP packet encoding, tap
names, quantities, wave sizing and the kind schedule, the residue rule and
unmeasured sources, host-netns detection, cgroup slack, the isolation policy,
agent output, the claim workload's write/verify/mismatch, pod and Endpoints
views, and finding the test's own image. qa-runner and must-gather have none. `cargo test` does not run the
test scripts or the suites, because they need a booted node.
The release profile uses `lto` and `strip`.

## How it ships

This repo produces **no golden** and is **not a stormcos component**. Nothing
from it is installed on a node. It is a stormcentral project in group `qa`,
and depends on `stormcos` and `stormblock-csi`.

- **The test container** is built and run by stormcentral's test runner
  (`stormcentral test run stormcos_qa <suite> [--commit …]`). On the build box it
  runs `test/build.sh`, then `podman build -f test/Containerfile .` from the repo
  root. It pushes the image to the test machine's registry
  (`<machine>:5100/test-stormcos_qa-<suite>:<commit12>`), then runs it as a Job
  in a run namespace that is deleted afterwards. Results are shown by
  `stormcentral test list` / `test show <run>`.
- **`qa-runner`, `must-gather` and the `tests/` scripts** run from a checkout,
  on any machine that can SSH to the node under test. Nothing runs them today
  (#14).

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
| `--masters <ip,ip,…>` | — | Master IPs. Sets `QA_MASTERS` (space-separated). Counts toward the `QA-Topology` gate. |
| `--nodes <ip,ip,…>` | — | Worker node IPs. Sets `QA_NODES` (space-separated). Counts toward the `QA-Topology` gate. |
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
   only with `--ssh`. `component` tests always run. A `cluster` test is also
   gated on `QA-Topology`: `single` always, `multi-node` (Tier 2) needs
   masters + nodes ≥ 3, `full` (Tier 3) needs ≥ 3 masters and ≥ 3 nodes;
   otherwise it prints `[SKIP]`. Skipped tests do not appear in the report.
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

## The test container: `/test short|medium|long`

One image for all three suites, per stormcentral's `docs/test-standard.md`:

```
test/build.sh                                   # static binary → test/out/test
podman build -f test/Containerfile -t stormcos_qa-test .
```

The image is `FROM scratch` with one static binary, `/test`, started as
`/test <suite>` (no argument: `$STORM_SUITE`). It does ssh (russh, with a
per-run ed25519 key) and the RDP probe in-process. The same image is also
the helper pods the suites start, so a run fetches nothing from outside the
machine:

- `/test serve [--port 8080]` answers every TCP connection on the port — a
  target to be reached, or not;
- `/test agent` probes a plan (`$PLAN`, key in `$SSH_KEY`) from inside a
  namespace and prints one JSON probe per line, then `{"agent":"done"}`;
- `/test claim` is the container waves' workload (see
  [Container waves](#container-waves-17)). Flags/env: `--token`/`CLAIM_TOKEN`
  (required), `--data`/`CLAIM_DATA` (`/data`), `--port` (8080),
  `--exit-once-after`/`CLAIM_EXIT_ONCE_AFTER` (0 = never).

Every suite prints one JSON object per line (`{"test","status","ms","detail"}`,
then `{"summary":…}`), also appended to `/results/<name>.jsonl`, and exits 0
(all passed or skipped), 1 (something failed) or 2 (could not run).
Environment, shared by the suites (each is also a flag):

| Env / flag | Default | |
|---|---|---|
| `STORM_SUITE` | — | the mode when `/test` gets no argument |
| `STORM_API` / `--api` | empty: in-cluster (`KUBERNETES_SERVICE_HOST`, the ServiceAccount's token and `ca.crt`) | apiserver URL |
| `--token-file` | the ServiceAccount's | bearer token file |
| `--insecure` | off | skip TLS verification (outside a cluster) |
| `STORM_NAMESPACE` / `--namespace` | the ServiceAccount's namespace | run namespace |
| `STORM_RUN_ID` / `--run-id` | `manual` (`long`: generated) | run label `storm.io/test-run` |
| `STORM_NODE` / `--node` | `127.0.0.1` (`medium`: empty) | the node under test: stormblock (`:9090`), RDP (`:3389`), and `medium`'s node/LAN targets |
| `STORM_TIMEOUT` / `--timeout` | 28800 s (`long`) | the window `long` fills with waves |
| `STORM_RESULTS` / `--results` | `/results` | output directory |

Outside a cluster: `--api https://<node>:6443 --insecure [--token-file f]`.
`/test <suite> --help` lists every flag; each suite's own flags are below.

[`test/requires.toml`](test/requires.toml) says what each suite needs beyond
the runner's namespace-only Role, in the format proposed in stormcentral#55.
The runner does not read it yet; until it does, a check that needs those
rights reports "could not run" (exit 2), never pass.

### short: prerequisites (`/results/short.jsonl`)

Under 2 minutes: `api` (the apiserver answers with the run's credentials),
`vm-resource` (`kubevirt.io/v1` VirtualMachines are served), `golden`
(`--golden`, `fedora-44-x86_64`, is on the node's stormblock; needs its
token, see below) and `helper-pod` (a `/test serve` pod from this image comes
up and answers; skip outside a pod without `--image`). Flags: `--golden`,
`--stormblock-url` (`http://<node>:9090`), `--image` (the helper pod's image;
default: this pod's own).

### medium: namespace isolation (#18, `/results/isolation.jsonl`)

The owner's ask: 5 intercommunicating VMs in a namespace with no outside
traffic. Isolation is exactly what stormconsole's "isolated namespace"
action applies, the NetworkPolicy `storm-isolate` (`podSelector: {}`,
ingress from and egress to same-namespace pods only), enforced by Cilium. It
covers a VM only once the VM is a real pod-network endpoint (stormvm#16).

A pod inside an isolated namespace cannot reach the apiserver, so the driver
(the Job, in the run namespace) stays **outside**. It uses the isolated
namespace `STORM_NAMESPACE_ISO` (default `<run ns>-iso`, created and
run-labelled if absent) and puts in it `--vms` (5) VMs on the pod network
(key in cloud-init user-data), `--pods` (2) `/test serve` pods, and a Secret
with the run's key. One more `serve` pod goes in the run namespace, as
"another namespace's pod". Probing from inside is done by an **agent** pod in
the isolated namespace, which probes every target over TCP, logs in to every
VM and probes from there (ICMP and TCP), and reports through its pod log.
The driver reads the log through the API.

VMs are `--golden` (`fedora-44-x86_64`), `--vm-memory-mib` 1024, logged in
to as `--ssh-user` (`fedora`); every probe waits `--probe-timeout` (3 s).

1. **Up.** Every VMI `Running` with an address, every pod `Running` with an
   IP (`--ready-timeout`, 900 s). A VM that does not come up fails `up/<vm>`
   with its VM/VMI in `/results/isolation-<vm>.json`.
2. **Control**, without the policy. The members must all reach each other;
   otherwise the pod network is broken, not isolated, and `control` fails.
   An outside target that cannot be reached even now tells nothing about the
   policy: it is reported **skip**, never pass.
3. **Isolate.** Create `storm-isolate`, then wait (`--enforce-timeout`,
   120 s) until the driver's own connections into the members go unanswered
   (`policy-enforced`).
4. **Isolated.** Probe again. `inside/<a>-><b>`: every member (and the
   agent) still reaches every other. `egress/<member>->{other-namespace-pod,
   node, lan, internet}`: blocked. The node is `STORM_NODE`:6443, the LAN
   `--lan-target` (default `<node /24>.1`), the internet
   `--internet-target` (`1.1.1.1:443`). With `STORM_NODE` empty there is no
   `node` target, and unless it is an IPv4 address (or `--lan-target` is
   given) no `lan` target; both are then silently left out, with no skip line
   ([#24](https://github.com/glennswest/stormcos_qa/issues/24)). `ingress/other-namespace-driver-><member>`:
   blocked.
5. **Clean up** everything it made (the runner also deletes run-labelled
   namespaces).

A probe's result is `open`, `refused` (some reply came back), `timeout`
(nothing: what a policy drop looks like) or `error`. Raw probes of each pass
are in `/results/isolation-{control,isolated}.json` and the agents' logs in
`/results/agent-*.log`.

Needs (requires.toml `[medium]`): the extra namespace `iso` from the runner
(stormcentral#55: the driver's namespace-only Role cannot create one), and
`kvm`. **Expected to fail until** stormvm#16 (a VM on the pod network has an
address the pod network can reach) and a Fedora golden on the node
(vmcloud-image-operator#15). **Not covered:** inbound from the node or the LAN
(no host-network vantage point under a namespace-only Role,
[#23](https://github.com/glennswest/stormcos_qa/issues/23)).

### long: the overnight soak in waves (#17, #16, `/results/long.jsonl`)

stormcentral's test standard ("Overnight soaks: waves"): a standing
`long`-suite test on **every** test machine, mixed hardware, sized from each
machine's own capacity, and "every night runs both kinds". `/test long`
takes the kinds in turn (`--kinds`, default `containers,vms`): wave 1 is
containers, wave 2 VMs, wave 3 containers, and so on. Each kind checks its
own prerequisites first (`containers/preflight`, `vms/preflight`), so a
machine that cannot run VMs (no golden, no VM resource, not enough memory
for 10) still runs the container waves.

It needs (requires.toml `[long]`) cluster read of `nodes` (wave sizing),
`pods` (free pod slots) and `persistentvolumes` (drain and residue),
`hostNetwork` (see below) and `kvm` (for the VM waves).

#### Container waves (#17)

The owner's ask: waves of pods and Deployments to capacity, each pod with a
stormblock claim, readiness and a Service; hold (restart, reschedule, write
and read the claim); drain (Deployments, pods, claims, PVs and volumes all
gone); repeat.

A stormblock claim is ReadWriteOnce, so a wave of N pods is **N Deployments
of one replica**, each with its own claim (`--claim-size-mib` 64, the
cluster's default StorageClass, which on stormcos is the built-in
`stormblock` driver; `--storage-class` overrides it) and a
`--pod-memory-mib` (16) memory request. The pods run this
image as `/test claim`, pinned to the node with a `kubernetes.io/hostname`
nodeSelector, with a TCP readiness probe on :8080. `/test claim` writes a
token and a 1 MiB blob to its claim, or checks them if they are there. It
logs `{"claim":"written"}`, `{"claim":"found"}` or `{"claim":"mismatch"}`,
and the driver reads that through the pod log API, so no data check needs
the pod network. After `--restart-after` (30 s) it exits once. A marker on
the claim makes that once per claim.

1. **Ready** (`wave-<k>/ready`). One Service for the wave, then a claim and a
   Deployment per pod. Every Deployment's pod becomes Ready. `create → Ready`
   is a pod's **start latency**, and `ready_all_ms` is the time until the
   whole wave is Ready.
2. **Service** (`wave-<k>/service`). The Service's Endpoints (or
   EndpointSlices) list every Ready pod, and its ClusterIP:8080 answers.
3. **Restart** (`wave-<k>/restart`). Every pod's container has exited once
   and been restarted in place by the kubelet. It is Ready again, and its log
   says the claim was `found`.
4. **Reschedule** (`wave-<k>/reschedule`). Every pod is deleted. Each
   Deployment's replacement pod comes up Ready and reads back the claim the
   old pod wrote.
5. **Drain** (`wave-<k>/drain`). The Deployments, the Service and the claims
   are deleted. Within `--drain-timeout`, none of the wave may be left: no
   Deployments, ReplicaSets, pods, claims or Services with
   `app.kubernetes.io/managed-by=stormcos_qa-containers`, no PV bound to the
   run's namespace, and no stormblock volume `pvc-<ns>-<claim>`.
   `drained_ms` is the time to drained.

A step's line lists the Deployments that failed it. The first 5 of them get
their Deployment, pods, claim and events in `/results/wave-<k>-<app>.json`.

**Wave size.** The smallest wave is `--min-pods` (10). The largest is
`--pod-fraction` (0.8) × the node's free pod slots: `status.allocatable.pods`
less the pods already bound to the node (if pods cannot be listed, all of
allocatable counts). `--pods N` fixes every container wave at N. A node with
fewer free slots than the smallest wave skips container waves.

#### VM waves (#16)

The owner's ask (#16): create 10 VMs, check their ssh and RDP ports, install
a package, restart them and check the package is still there, delete them,
and repeat.

**A wave** (every VM in a wave runs concurrently):

1. **Ramp.** Create N `VirtualMachine`s (`kubevirt.io/v1`) in
   `STORM_NAMESPACE`, labelled `storm.io/test-run=<STORM_RUN_ID>`. Each
   clones its root disk from `--golden` (`fedora-44-x86_64`), gets the run's
   key through **`accessCredentials`** (a Secret, `noCloud` propagation), is
   bridged onto `--bridge` (`storm.io/bridge: stormbr0`), has a graphics
   device (for RDP), and is pinned to the node under test with a
   `kubernetes.io/hostname` nodeSelector, so the scheduler places it (a VMI
   with only `spec.nodeName` is never picked up by rustkube-node's kubelet).
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

VMs get `--vm-cores` (1) and `--vm-memory-mib` (2048) and are logged in to as
`--ssh-user` (`fedora`). The install has `--install-timeout` (600 s); every VM
and pod has `--ready-timeout` (900 s) to come up. stormvm is read at
`--stormvm-url` (`http://127.0.0.1:9095`), RDP at `--rdp` (`<node>:3389`),
stormblock at `--stormblock-url` (`http://<node>:9090`), and host counters
under `--proc-root` (`/proc`).

**Wave size.** The first VM wave is the smallest (`--min-vms`, 10), because it is the
latency baseline. The largest is `--capacity-fraction` (0.8) × the Node's
`status.allocatable.memory` ÷ `--vm-memory-mib` (2048). It is also capped by
the host's `MemAvailable`, less 1 GiB, at guest memory plus 10%. Later waves
vary across that range. Waves repeat until `STORM_TIMEOUT` (less a drain
reserve) would be overrun, or `--waves N` (alias `--cycles`). `--vms N` fixes
every VM wave at N. If the machine cannot hold the smallest wave, VM waves
report **skip**, not pass.

#### Both kinds

Waves repeat until `STORM_TIMEOUT` (less a drain reserve) would be overrun,
or `--waves N` (alias `--cycles`, all kinds together). Each kind's sizes
vary across its range, starting with its smallest.

**Residue and slowdown.** A census is taken before the first wave and after
every drain:

| metric | source |
|---|---|
| stormblock volumes / attachments | `<node>:9090/api/v1/volumes`, `…/{id}/attach` |
| stormvm registrations | `127.0.0.1:9095/api/v1/vms` (loopback-only on stormcos, hence `hostNetwork`) |
| taps | the host's `/proc/net/dev` (no API lists them) |
| pod veths (`lxc*`, `veth*`) | the host's `/proc/net/dev` |
| cgroups | `/proc/cgroups`, the largest `num_cgroups` (host-wide) |
| PersistentVolumes | `GET /api/v1/persistentvolumes` |
| node memory in use | `/proc/meminfo` (`MemTotal − MemAvailable`) |
| allocated file handles | `/proc/sys/fs/file-nr` |

A wave **fails** in any of these cases:

- a VM or a Deployment failed any step;
- anything of the run is left after the drain;
- a metric is above the baseline by more than its slack **and** above the
  previous drain, so it is still growing and not a one-off plateau. The slack
  is 0 for counts, `--cgroup-slack` 16, `--mem-slack-mib` 512 and
  `--fd-slack` 2048;
- its median start latency is more than `--slowdown` (1.5) × that of the
  first wave of its kind, plus `--slowdown-grace-secs` (30).

A source that cannot be read is reported as unmeasured (`null`), never as 0. Taps and veths
are counted only when `/proc/net/dev` is the host's (the VMs' bridge,
`cilium_host` or `lxc_health` is in it). A pod's own namespace would read
as a wrong 0.

**A drain cannot pass unchecked.** If the baseline census cannot read a
leftover source that the planned waves' drain relies on, the run reports
`residue/<source>` as `could not run`, and the exit code is 2 unless
something failed. The sources are: `stormblock` (volumes: its token is
missing or refused), `stormvm` (registrations, VM waves: no `hostNetwork`),
`host-network` (taps and veths) and `persistentvolumes` (container waves: no
cluster read). Under stormcentral's runner today, the Job has neither
stormblock's token nor `hostNetwork` (stormcentral#55), so `long` reports
2 there even when every wave passes.

**Output**, per test-standard.md:

- one JSON object per line on stdout, also appended to
  `/results/long.jsonl`. There is a `<kind>/preflight` line per kind. A VM
  wave has a line for each step, `wave-<k>/<vm>/{create,up,rdp,install,restart}`;
  a container wave has `wave-<k>/{service,ready,restart,reschedule}`. A wave
  cut off by the end of the window gets `wave-<k>/hold` (fail). Every
  wave has a `wave-<k>/drain` line, and a `wave-<k>` line whose `wave` field
  holds the wave's record (kind, size, latencies, `drained_ms`, residue,
  leftovers, regressions). The last line is `{"summary":…}`;
- the trend in `/results/waves.json`;
- a failing VM's VM, VMI and events in `/results/wave-<k>-<vm>.json`, and a
  failing Deployment's in `/results/wave-<k>-<app>.json`.

The exit code is 0 when everything passed or was skipped, and 1 when
something failed. It is 2 when a part could not run and nothing failed:

- the whole test cannot run when the apiserver is unreachable or refuses, or
  the node under test cannot be identified;
- the VM waves cannot run when the VirtualMachine resource is not served or
  the golden is not in the node's stormblock;
- the container waves cannot run when the test cannot learn its own image
  (outside a pod: pass `--image`).

A kind that could not run is a `<kind>/preflight` line that says
`could not run`, and the other kind still runs.

**Environment:** `STORM_API` (empty means in-cluster), `STORM_NAMESPACE`,
`STORM_RUN_ID`, `STORM_NODE` (the node's address or name; stormblock and
RDP are reached at it), `STORM_TIMEOUT` (8 h if unset) and `STORM_RESULTS`
(`/results`). Run `/test long --help` for every flag. Outside a cluster,
use `--api https://<node>:6443 --token-file <f> --insecure`.

stormblock (v17, stormblock#107) answers volume calls only with its bearer
token. `long` and `short` find it the way stormblock's CLI does:
`STORMBLOCK_API_TOKEN`, the file at `STORMBLOCK_TOKEN_FILE`,
`/etc/stormblock/api_token`, `/var/lib/stormblock/api_token`. Without it the
golden check cannot tell (`long` warns and goes on with volume residue
unmeasured, and then reports `residue/stormblock` could not run; `short`
fails `golden` saying stormblock refused).

`--seed-key` also puts the key in the cloud-init user-data. It is a
diagnostic bypass while stormvm#41 is open, so that the later steps can be
measured. It is **not** a pass of #16.

**Expected to fail until** these are fixed:

- stormvm#40 (a bridged VMI reports no address);
- stormvm#41 (`accessCredentials` is not read);
- stormrdp#1, stormcos#69 (stormrdp is not on the node yet);
- stormvm#22 (a restart re-clones the root disk, so the package is lost);
- vmcloud-image-operator#15 (the Fedora golden never reaches the node; its
  fix ships with stormcos#147).

rustkube-node#35 (delete stops the VM), listed here before, is closed.

Passing 10 × 10 is the definition of done for that set.

**Not yet:**

- the first failure's console log. It is only reachable as a WebSocket
  (`…/virtualmachineinstances/<vm>/console`), and the soak does not read it;
- `requires: [kvm]` is declared, but no Node advertises KVM through the API
  yet, so only the runner can match it;
- a run on a node. stormcentral's runner runs a suite on request, but no run
  of this image has reached C2NR0Q2 yet (#16, #17), and under the runner
  `long` has neither cluster read, `hostNetwork` nor stormblock's token
  (stormcentral#55).

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
