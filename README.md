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
  `/test short|medium|long`. `short` is the smoke test (node Ready, platform
  pods up) and checks what the VM suites stand on,
  `medium` is namespace isolation (#18), `long` is the overnight soak in
  waves of containers (#17) and VMs (#16), and `/test container-waves` is
  its container waves alone, sized for a day run (#17); `/test vm-waves` the
  same for its VM waves (#16). `/test turbomode` is the
  explicit load test (#26): 100 Pods, then 25 Pods with a SQLite claim
  each, with a storage audit on the node, in 15 min; `/test turbomode-night`
  is the full scale, 1,000 and 100, for the night window (#43). See [below](#the-test-container-test-shortmediumlongturbomode);
- **`must-gather`**, the debug-data collector, run from a workstation or a
  pod: cluster state and pod logs through the API, each node's host data
  through a short-lived collector pod (no ssh, #45), plus the collector
  scripts that components put in `gather/<area>/`.

`qa-runner`, `must-gather` and the test binary are command-line tools you run
once and they exit. None of them has a port of its own, a health endpoint or
metrics. The test binary's helper modes (`serve`, `claim`) listen on TCP
8080 inside their own pods, as a probe target.

> **Status, 2026-10-03.** The **test container** is what stormcentral runs:
> `stormcentral test run stormcos_qa <suite>` builds the image and runs it as a
> Job on a test machine (see [How it ships](#how-it-ships)). The first runs
> to reach a node were on pvetest1 (11.72) on 2026-10-03: `short` (4a3e735d59)
> ran and reported, and `turbomode` (7f7d36b3c2) ran with its cluster read
> and host access but died with SIGSEGV (exit 139) after about an hour (#26).
> The pod log the runner reads loses the start of most lines
> (rustkube-node#136), so results go missing until that is fixed.
> Those crashes were the platform (stormblock#267). On 11.78 the day-sized
> `turbomode` **passed** (7277704177, pvetest1, both profiles on attempt 1,
> cleanup and the storage after-audit verified, image unchanged).
> **Nothing runs `qa-runner` or the `tests/`
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
crates/qa-test/             the test container's binary: /test short|medium|long|turbomode
                            and helpers serve|agent|claim|sleep|sqlite
test/build.sh               builds that binary static (musl) into test/out/test
test/Containerfile          the test image (scratch + /test)
test/requires.toml          what each suite needs beyond a namespace-only Role (stormcentral#55, #74)
tools/turbomode/            the Python reference of turbomode and its selftest (run by hand)
tools/symbolize-crash.sh    names the addresses of a /test crash report (on the build box)
```

The test directories today are `ironprom`, `overall`, `rustkube`,
`rustkube-node`, `stormblock`, `stormblock-csi`, `stormcos` and
`topology/single`. fastetcd's checks of a running fastetcd (health,
round-trip, scale) are its own test container's (`stormcentral test run
fastetcd short|medium|long`); the scripts here went when :2379 became mutual
TLS (#51, stormcos#146). The collector directories are `fastetcd`, `ironprom`,
`kernel`, `stormblock` and `stormblock-csi`.

## Build

Workspace version `0.1.0`, Rust edition 2024. Build on the build box, never on
this VM and never as root:

```bash
git push && sc-build        # cargo build && cargo test on dev.g8.lo, scratch dir
```

`cargo test` runs `qa-test`'s 60 tests: RDP packet encoding, tap
names, quantities, wave sizing and the kind schedule, the residue rule and
unmeasured sources, host-netns detection, cgroup slack, the isolation policy,
agent output, the claim workload's write/verify/mismatch, pod and Endpoints
views, finding the test's own image, and turbomode's retry rule,
percentiles, Pod observer, SQLite workload and evidence check, and storage
audit. Seven of them run the turbomode driver end to end against an
in-process fake apiserver and stormblock (`turbomode_fake.rs`: a clean
run, a lost claim ack retried, corruption final, a leaked volume, Pods that
never finish timed out, cleaned up and retried, 1,000 Pods of which 250 finish, no node).
must-gather has 13: the log frame (noise, the #136 cut, a lost line, a cut
log), the collector pod spec, secrets listed not copied and redacted, a
bundle from a fake host unpacked through the frame, CRD lists, which pods'
logs, the manifest inside the tarball. `/test must-gather` checks its
bundle (1 test). qa-runner has none. `cargo test` does not run the
test scripts or the suites against a cluster, because they need a booted node.
`python3 tools/turbomode/selftest.py` runs the Python reference's 24 self-tests.
The release profile uses `lto` and `strip = "debuginfo"` (the symbol table stays, for the crash report).

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
6. **Gather** (with `--gather`). This runs must-gather with the run's `--api`
   and `--out <artifacts>/must-gather-<release>`. must-gather refuses
   `http://` (the default `--api` here, #11), and qa-runner has no token or
   `--host-image` to pass, so give it an https `--api` and a
   `MUST_GATHER_TOKEN_FILE` in the environment.
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

## The test container: `/test short|medium|long|turbomode`

One image for all three suites, and the explicit `turbomode` load test, per stormcentral's `docs/test-standard.md`:

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
  `--exit-once-after`/`CLAIM_EXIT_ONCE_AFTER` (0 = never);
- `/test sleep [secs]` and `/test sqlite` are [turbomode](#turbomode-the-load-test-26-resultsturbomodejsonl)'s
  workloads (the image has no `sleep` or `python3`). `sqlite`:
  `--pod-uid`/`POD_UID` (required), `--data`/`SQLITE_DATA` (`/data`),
  `--sleep`/`SQLITE_SLEEP` (120).
- `/test crash` faults on purpose, to check the crash report.

**A crash names its place.** On SIGSEGV or SIGBUS, every mode first writes a
report to stderr: `crash:signal=…`, `crash:addr=…`, `crash:anchor=…`,
`crash:rip=…`, `crash:rsp=…`, one `crash:ret=…` per frame-pointer frame, and
`crash:thread=…`. Then the process ends with the signal as before (exit
139). The lines have no spaces, because the kubelet's `/log` strips the
first three words of a line with three or more (rustkube-node#136). Run
`sc-build 'tools/symbolize-crash.sh < tmp/crash.txt'` at the crashed commit
to name the addresses. `test/build.sh` builds with frame pointers and
remapped paths, so the rebuild is the same binary.

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
| `STORM_TIMEOUT` / `--timeout` | 28800 s (`long`, `container-waves`, `vm-waves`; the runner gives them 900 and 1800), 900 s (`turbomode`; give `turbomode-night` 14400 by hand); the runner sets the suite's budget | the window `long` fills with waves; a `turbomode` attempt starts only if its worst case fits in it |
| `STORM_RESULTS` / `--results` | `/results` | output directory |

Outside a cluster: `--api https://<node>:6443 --insecure [--token-file f]`.
`/test <suite> --help` lists every flag; each suite's own flags are below.

[`test/requires.toml`](test/requires.toml) says what each suite needs beyond
the runner's namespace-only Role, in the format proposed in stormcentral#55.
The runner does not read it yet; until it does, a check that needs those
rights reports "could not run" (exit 2), never pass.

### short: prerequisites (`/results/short.jsonl`)

Under 2 minutes: `api` (the apiserver answers with the run's credentials),
the smoke test (#34): `node-ready` (every Node `Ready=True`; the detail names
the ones that aren't, with their conditions) and `system-pods` (every
platform pod Running with all containers Ready, or Succeeded, and restarted
at most `--max-restarts` (3) times, so a crash-looping pod that happens to be
Running still fails; the detail lists each one that isn't up with phase,
reason, ready count and restarts). Platform pods are every pod outside the
runner's test namespaces (labelled `storm.io/purpose=test`) and not labelled
`storm.io/test-run`: on a test machine that is the release's own,
kube-system among them. Both wait up to `--settle` (30 s) for a clean answer
and need `[short]`'s `cluster_read` (nodes, namespaces, pods); refused, they
report `could not run` and `short` exits 2 unless something else failed.
Then `vm-resource` (`kubevirt.io/v1` VirtualMachines are served), `golden`
(`--golden`, `fedora-44-x86_64`, is on the node's stormblock; needs its
token, see below) and `helper-pod` (a `/test serve` pod from this image comes
up and answers; skip outside a pod without `--image`). Flags: `--golden`, `--max-restarts`, `--settle`,
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

**By day: `/test container-waves`.** `long` runs 8 h, so stormcentral runs it
only in the night window on a pve VM (stormcentral#325). `container-waves` is
the same driver with `--kinds containers --waves 3 --max-pods 20` put before
the caller's flags (a flag given still wins): three waves of 10, 20 and 15
pods, in `[container-waves] budget_secs = 900`. It needs no kvm, so it runs
on bare metal too: `stormcentral test run stormcos_qa container-waves --tag
<machine>`. `--max-pods N` caps a wave of the night suite the same way.

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

**By day: `/test vm-waves`.** The same driver with `--kinds vms --waves 2
--min-vms 2 --max-vms 10 --ready-timeout 600 --install-timeout 300
--drain-timeout 240` before the caller's flags: a wave of 2 VMs, then one of
up to 10 (as many as the node's memory holds), in `[vm-waves] budget_secs =
1800`, the most a day run may take. Needs kvm, hostNetwork and the
stormblock token file, like `long`. `--max-vms N` caps a VM wave of the
night suite the same way.

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
| stormblock slab allocation | `<node>:9090/api/v1/slabs`: Σ `allocated_slots` × `slot_size` (and slots), so space a deleted volume never gave back shows while the volume count is flat (#36) |
| stormblock engine memory | `VmRSS` in the host's `/proc/<pid>/status` of `stormblock … adopt-ublk` (`hostPID`; stormblock exports no RSS, and node memory can hide it behind a guest's page cache, #36) |
| the run's Secrets | `GET /api/v1/namespaces/<ns>/secrets`: any but the run's own key (a VM's cloud-init is inline `userData`; nothing may leave a seed Secret) — must be none after a drain (#36) |

A wave **fails** in any of these cases:

- a VM or a Deployment failed any step;
- anything of the run is left after the drain;
- a metric is above the baseline by more than its slack **and** above the
  previous drain, so it is still growing and not a one-off plateau. The slack
  is 0 for counts, `--cgroup-slack` 16, `--mem-slack-mib` 512,
  `--fd-slack` 2048, `--slab-slack-mib` 0 and `--engine-mem-slack-mib` 256;
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
`host-network` (taps and veths), `persistentvolumes` (container waves: no
cluster read), `stormblock-slabs` (the volume list answers but the slab
list does not) and `engine-memory` (the engine's process is not visible: no
`hostPID`; `long`, `container-waves` and `vm-waves` declare `host_pid`). Under stormcentral's runner today, the Job has neither
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
`/etc/stormblock/api_token`, `/var/lib/stormblock/api_token`, then the
engine's own `/run/stormblock/engine/api_token` under `STORM_HOST_ROOT` (the
runner mounts that one file read-only for `short`, `long`, `container-waves`,
`vm-waves` and `turbomode`,
stormcentral#74) or at `/`. Without it the
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

### turbomode: the load test (#26, `/results/turbomode.jsonl`)

Explicit, never part of `short|medium|long`. The owner's choice on #33
(option A): the driver, the storage audit and the SQLite workload are all
in this image, and every Pod and claim goes in the run namespace.

**Two sizes** (#43; owner: a golden's test fits 15 min by day, anything
over 30 min runs only at night on a pve VM, stormcentral#325). The same
driver; `turbomode-night` puts its flags first, so a flag given still wins:

| suite | `requires.toml` budget | sleeping Pods | SQLite pairs | sleep | attempts | finish / cleanup timeout |
|---|---|---|---|---|---|---|
| `turbomode` (day) | 900 s | 100 | 25 | 60 s | 2 | 240 s / 120 s |
| `turbomode-night` | 14400 s | 1000 | 100 | 120 s | 3 | 3600 s / 600 s |

100 Pods stay under the ~250 pod-IP plateau, so only the night suite would
catch a return of rustkube-node#137. Two profiles, one after the other
(`--profiles`, default `sleep,sqlite`); the numbers below are the day
defaults:

- **sleep**: `--sleep-pods` (100) Pods running `/test sleep 60`
  (restartPolicy Never); every one must reach Succeeded;
- **sqlite**: `--sqlite-pods` (25) Pods, each with its own fresh claim
  (`--claim-size` 64Mi, `--storage-class` or the cluster's default, which
  must reclaim with Delete and is never changed) running `/test sqlite`:
  1,000 Pod-specific 1 KiB records written in one FULL-sync transaction,
  read back read-only with per-record SHA-256 and `PRAGMA
  integrity_check`, a `--sleep-seconds` sleep, checked again. SQLite is built into the
  binary. Each Pod's log (`{"phase":"verified"}`, `{"phase":"complete"}`)
  is the evidence, and is kept.

The **storage audit** runs in-process, read-only, on the node (owner,
#26): stormblock's volumes and slab slots for the run's claims (by the
built-in driver's `pvc-<ns>-<claim>` name and the PVs' volume handles),
every host process's mountinfo and cgroup lines, host init's mountinfo,
and the cgroup tree, all searched for the run's Pod UIDs, volume ids and
names. `before` records; `allocated` needs exactly the run's volumes, each
with storage allocated; `after` needs nothing of them left. Per-Pod kubelet
directories are recorded `unmeasured` unless `--kubelet-pods` is mounted.
The cluster must have exactly the Job's node (one Job sees one node).

**Attempts** (`--attempts` 2, at most 5; `--retry-delay` 30 s): each is a
fresh run with its own label `qa.storm.io/turbomode-run=<token>`, its own
evidence in `<results>/turbomode/<profile>/attempt-N/` (`report.json`,
`pods.json`, `<pod uid>.log`, `storage-<phase>.json`), and
`<results>/turbomode/<profile>/summary.json` lists every attempt. Only a
*transient* failure (a create error incl. a lost ack, not every Pod
finished in `--finish-timeout` 240 s, a sleeping Pod failed) is retried,
and only when that attempt's cleanup verified, including the `after`
audit. Integrity, storage, cleanup and unexpected failures are final, so a
pass never hides corruption or a leak; a pass after retries says so
(`retried`, `passed_on_attempt`). Counts never change between attempts.

**Cleanup** always runs: this attempt's Pods and claims, found by label too
(a create whose ack was lost), deleted with UID preconditions; then it
waits up to `--cleanup-timeout` (120 s) for them, their PVs and
VolumeAttachments to be gone. It never removes finalizers or deletes PVs
or volumes itself. Latency (request → scheduled/Running/finished, create
ack, claim request → Running; p50/p95/p99/max) comes from a Pod watch; a
watch error relists and marks the attempt's latency invalid. The peak of
Running Pods is recorded: the node's pod capacity, not the test, bounds
how many of them run at once.

One line per profile, `turbomode/sleep` and `turbomode/sqlite`; when the
whole suite cannot run, one `turbomode/preflight` line instead. Could not
run (exit 2, never a pass): the test's own image unknown (outside a pod:
pass `--image`), no cluster read of nodes, persistentvolumes or
volumeattachments; for `sqlite` only (the `sleep` profile still runs), StorageClasses unreadable, no default
StorageClass or one that does not Delete, no `TURBOMODE_NODE`, another node in the cluster, or the Job's
host access missing (not hostPID, no host mountinfo, cgroup tree or
stormblock token). The Job's host access (`test/requires.toml`
`[turbomode]`), all read-only:

| Env / flag | Default | |
|---|---|---|
| `TURBOMODE_NODE` / `--node-name` | — | downward API `spec.nodeName` |
| `STORM_HOST_ROOT` / `--host-root` | `/` | where the runner mounts the host paths read-only (`/host`, stormcentral#74) |
| `TURBOMODE_PROC` / `--proc-root` | `<host root>/proc` | host /proc (hostPID) |
| `TURBOMODE_HOST_MOUNTINFO` / `--host-mountinfo` | `<proc>/1/mountinfo` | host init's, through `/proc` |
| `TURBOMODE_CGROUP` / `--cgroup-root` | `<host root>/sys/fs/cgroup` | hostPath, read-only |
| `TURBOMODE_TOKEN` / `--stormblock-token` | `<host root>/run/stormblock/engine/api_token` | that file only, not `/run` |
| `TURBOMODE_STORMBLOCK` / `--stormblock-url` | `http://<node>:9090` | stormblock's API |
| `TURBOMODE_KUBELET_PODS` / `--kubelet-pods` | unset: unmeasured | kubelet pods dir |
| `--image-file` | `/test` | the test's own executable, re-read from its volume (cache dropped) after every attempt; a change is a final integrity failure (stormblock#267). Absent: not checked |

Other flags: `--sleep-seconds` (60), `--concurrency` (32 creates in
flight), `--image` (default: the Job pod's own), `STORM_TIMEOUT` (default
900 s, the suite's `budget_secs`; the runner makes it the Job's deadline,
plus 180 s, so an attempt starts only if its worst case — finish + cleanup
timeouts + 120 s, 480 s by day and 4,320 s at night — still fits;
otherwise the profile reports could not run, or stops retrying).

**Progress** goes to stderr, one line per step of each attempt (start,
creates issued, Pods finished once a minute, finish timeout, cleanup and its
outcome). Results go to stdout only when a profile ends, so after a crash
these lines show how far the attempt got.

**Under the runner:** it starts the suite with its budget
(stormcentral#247), the declared host access (hostPID, the read-only host
paths at `/host<path>`, the node name: stormcentral#74) and the cluster
reads (stormcentral#55). The first live runs (7f7d36b3c2, 097feaf43d,
pvetest1, 2026-10-03) died with SIGSEGV after about an hour, between the
sleep profile's first finish timeout and the start of its cleanup; only 250
of the 1,000 Pods had finished: Succeeded Pods keep their pod IPs and the
node's Cilium range fills (rustkube-node#137), and the scheduler binds past
the node's 110 allocatable Pods (rustkube#194). Since then the binary drives every suite on
its own 64 MiB `suite` thread (not the main thread) from a boxed future, on
tokio threads with 16 MiB stacks: a stack overflow there is reported, where
on a static musl binary's main thread it is a bare SIGSEGV. The third run
(4bbb76be8f) still died with 139, inside cleanup's first LIST of the 1,000
Pods, with no overflow reported, so the binary now prints a crash report
(above). No passing live run of `main` has been recorded yet. The crashes were the platform: a debug
build that re-read `/test` from its volume each minute saw its bytes change
once the sqlite profile's 100 volumes were being written (run 7260943051,
stormblock#267). A running executable whose code changes under it crashes
anywhere, which is what every run showed. turbomode now checks its image
after each attempt and reports a change as an integrity failure. A debug build (5-minute finish timeout,
run 1260fd7c36) passed `turbomode/sqlite` on pvetest1, cleanup and storage
audit included. `turbomode/sleep` cannot pass while finished Pods keep their
IPs (rustkube-node#137). rustkube keeps a watch open past `timeoutSeconds`
(rustkube#165), and the observer resumes after the client's timeout with no
gap. `tools/turbomode/` (the
Python `run.py`, workload and auditor) stays as the reference and its
selftest oracle; the Rust driver has its own end-to-end tests against a
fake apiserver and stormblock (`turbomode_fake.rs`).

## must-gather

Storm CoreOS's debug-data collector, run from a workstation (the usual
case) or from a pod. **No ssh**: a stormcos node has none for this (ssh lands
in the `fedora` container, not on the host), and its services run under
stormpump, not systemd (#45). How it ships to customers is #48.

```
must-gather --api https://<node>:6443 --token-file <f> [--ca-file <ca> | --insecure] \
            [--host-image <image>] [--out <dir>] [--nodes <n1,n2>] [--collectors-dir gather]
```

| Flag | Default | Effect |
|---|---|---|
| `--api <url>` (`MUST_GATHER_API`) | in-cluster | The apiserver. `http://` is refused: rustkube serves TLS (#11). Empty: the pod's own (`KUBERNETES_SERVICE_HOST`). |
| `--token-file <f>` (`MUST_GATHER_TOKEN_FILE`) | the pod's ServiceAccount token | Bearer token, re-read on every request. |
| `--ca-file <f>` (`MUST_GATHER_CA_FILE`) | the pod's ServiceAccount CA | The cluster's CA. |
| `--insecure` | off | Do not verify the apiserver's certificate. |
| `--out <dir>` | `must-gather-<unix time>` | Output directory; `<out>.tar.gz` is written next to it. |
| `--nodes <a,b>` | every node | Nodes for the host part and the collector scripts. |
| `--host-image <ref>` (`MUST_GATHER_HOST_IMAGE`) | — | The image the per-node collector pod runs; it must hold must-gather at `--host-command` (`/must-gather`). Without it the host part is skipped and the manifest says so. Until must-gather has a golden (#48), the stormcos_qa test image carries it. |
| `--host-namespace <ns>` | a new `must-gather-<run>` | Where the collector pods run. A created namespace is labelled `pod-security.kubernetes.io/enforce=privileged` and deleted afterwards. |
| `--host-timeout <s>` | `300` | How long a node's collector pod may take. |
| `--collectors-dir <dir>` | — | Component collector scripts, `<dir>/<area>/*.sh`. |
| `--timeout <s>` | `60` | Each script's timeout. |

**What it collects:**

- `cluster/`, through the API: `version`, `healthz`, `livez`, `readyz`,
  `api`, `apis`; every object of nodes, namespaces, pods, events, services,
  endpoints, PVs, PVCs, service accounts, deployments, daemonsets,
  statefulsets, replicasets, jobs, storage classes, volume attachments, CSI
  drivers and nodes, leases, network policies, CRDs and priority classes
  (paged, one List per file); and every custom resource of every CRD in
  `cluster/crs/<plural>.<group>.json` (VMs, VMIs, CloudImages, placements,
  stormblock's, …). **Never Secrets or ConfigMaps.**
- `logs/<ns>/<pod>/<container>[.previous].log`: every kube-system pod (the
  node's own services, fastetcd, the apiserver, rustkube-node and the rest,
  are mirror pods `kube-system/<service>-<node>` there) and every pod that
  is not Running and Ready, or has restarted (the previous log too); last
  5,000 lines, at most 4 MiB each. These pass through rustkube-node's log
  filter, which cuts the first three words of a line with 3+ spaces
  (rustkube-node#136); the host bundle has the same logs unfiltered.
- `nodes/<node>/host/`, per node: a pod pinned with `spec.nodeName`, with
  `hostPID`, `hostNetwork` and read-only mounts of `/run/stormpump`,
  `/sys/fs/cgroup`, `/l`, `/var/log/pods`, `/dev`, `/sys/fs/pstore`,
  `/etc/stormpump`, `/state/config` and stormblock's engine token, runs
  `must-gather host-collect`. Its bundle: the host's `/proc` (version,
  cmdline, mountinfo, meminfo, pressure, …) and `/proc/net`; the kernel log
  (`/dev/kmsg`, read without consuming it), pstore, `/dev` and sysfs
  listings, the block-device map, the cgroup tree; stormpump's
  `assets.json`, `runs.tsv` and logs; stormd's service logs (`/l`); every
  pod log file as written; `/etc/stormpump` and `stormcos.toml` with
  secret-looking values redacted, and a listing of `/state/config`;
  stormblock's engine API (health, volumes, slabs, drives, arrays, pallets,
  exports, rebuilds, debug, metrics) and fastetcd's metrics (`:2381`);
  `errors.txt` for anything it could not read. The kubelet has no exec and
  the apiserver no node proxy, so the bundle comes back through the pod's
  log as base64 lines between `mg:begin:<bytes>:<sha256>` and `mg:end`
  (no spaces, so #136 cannot touch it), checked on arrival. **Secrets stay
  on the node**: keys, tokens, pull secrets and `authorized_keys` are listed
  with their size, never copied.
- `nodes/<node>/collectors/<area>/<script>.txt`: the component scripts.
- `manifest.json`: every item with `ok`, `error` (and why) or `skipped`, and
  the counts. It is written before the tarball, so it is in it (#13).

**Component collectors** are `gather/<area>/*.sh`, run with `sh` where
must-gather runs (not on the node), once per node, with `QA_API` (https),
`QA_TOKEN_FILE`, `QA_CA_FILE`, `QA_INSECURE` (`1`/`0`), `QA_NODE` and
`QA_NODE_IP` set. Today:

| Script | Collects |
|---|---|
| `gather/cadvisor/status.sh` | cadvisor's `/healthz`, version, attributes, machine, container names, last 100 events, its version/machine metrics and `container_*` series count on `$CADVISOR_ENDPOINT` (default `http://<node>:9096`, optional `CADVISOR_TOKEN`); stormd's `/healthz`, `process="cadvisor"` metrics, `/api/v1/processes/cadvisor` and the last 300 log lines on `$CADVISOR_STORMD` (`:9196`, optional `STORMD_TOKEN`). The cgroup tree and disk map are in the host bundle. Owner: glennswest/cadvisor |
| `gather/ironprom/status.sh` | through the API: ironprom's pods and StatefulSets in `$IRONPROM_NAMESPACE` (`monitoring`); then, if the pod address is reachable from here, buildinfo, `status/tsdb`, `status/runtimeinfo`, targets, rules and `ironprom_*` self-metrics |
| `gather/stormblock-csi/status.sh` | through the API: pods in `stormblock-system`, `wanderingvolumes` and `volumepolicies`, the `stormblock-tiebreak` and node leases, `csistoragecapacities` |

The old ssh-based `fastetcd`, `kernel` and `stormblock` scripts are gone:
the host bundle has what they read (fastetcd's metrics and logs, ublk and
io_uring state, stormblock's engine).

**Tested** by `/test must-gather` on a test machine: it runs must-gather
in-cluster with this image as `--host-image`, then checks the manifest, the
cluster lists, each node's host bundle (stormpump status, `/proc`, the
kernel log, `/proc/net`, stormblock's volume list, cgroups, service and pod
log listings), that no key, token or pull secret was copied, and that no
collector pod is left. `[must-gather]` in `test/requires.toml` declares its
reads; the runner grants no subresource, so kube-system's `pods/log` is 403
there and counted as not granted, not failed.

## Adding tests and collectors

Put executables in `tests/<your-repo>/` and follow [STANDARD.md](STANDARD.md).
Put collector scripts in `gather/<area>/*.sh`. They run where must-gather
runs, with `QA_API` and a token: see `gather/stormblock-csi/status.sh` for
the `api` helper. Anything only the node has belongs in must-gather's host
collector (`crates/must-gather/src/host.rs`).
