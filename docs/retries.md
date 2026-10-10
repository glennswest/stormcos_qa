# Retries: every call that leaves the process (#57)

Owner (#57, 2026-10-08), after a test machine's images failed on one 30 s
engine timeout: "The flaky sounds more like a test problem … sounds like
it doesn't retry. And that needs a code review, to make sure we have
retries." This is that review: the shared helper, its defaults, and every
remote call site, with what it did before and what it does now.

## The helper: `crates/retry` (`stormcos-qa-retry`)

One crate, used by both binaries (`qa-test`'s `/test`, `must-gather`).

- `retry::with_backoff(policy, what, op)`: a call that either works or
  fails. Per attempt, `op` says whether a failure is `Step::Transient`
  (worth another try) or `Step::Final` (a real answer, returned at once).
  The wait after attempt *n* is `first_delay · 2^(n-1)`, capped at
  `max_delay`, jittered into its upper half. A `Retry-After` sets a floor.
  The retries stop at `attempts` or `deadline`, whichever comes first: no
  retry starts if its wait would end past the deadline. Giving up returns a
  `retry::Infra` error.
- `retry::probe(policy, what, op)`: a reachability probe. "No answer" is
  itself a result, so it is tried again until something answers or the
  policy runs out. It returns the last result and the number of attempts.
- `retry::http::send` / `open`: HTTP through `with_backoff`.
  - **Transient:** transport errors (refused, reset, timeout) and the
    statuses 408, 429, 500, 502, 503 and 504.
  - **Real answers:** every other status, returned to the caller. So is a
    certificate the client refuses (`invalid peer certificate`,
    `retry::http::certificate_refused`): another try gets the same
    certificate, so it fails at once, as a failure and not "could not run"
    (#59: `short`'s `sc-login` reports it as a CA mismatch).
  - **`Repeat::Safe`** (GET, DELETE, opening a watch): retried on any
    transient failure.
  - **`Repeat::Unsafe`** (POST, a subresource PUT): retried only when the
    server certainly did not act, meaning the connection never opened or
    the answer was 408, 429 or 503. Any other failure may have landed. It
    fails at once as an `Infra` marked `unsure`, and the caller decides.
- **Logged on stderr.** stdout is the test's JSON lines. The messages are:
  - `retry: GET /api/v1/nodes: attempt 2 failed (answered 503: …); next in 1.3 s`
  - `retry: GET /api/v1/nodes: succeeded on attempt 3 after 4.1 s`
  - `retry: GET /api/v1/nodes: gave up after 5 attempts / 1 min 52 s: <last error>`
  - a probe logs only `answered on attempt n after …`, because its
    attempts are already in its result.
- **Classified.** `retry::is_infra(&err)` is true when the helper gave up
  on infrastructure (anywhere in the error chain). `report::Out::failure`
  reports such an error as `could not run: …`, and `Out::exit_code` gives
  0 (all passed), 1 (something really failed) or 2 (only "could not run").
  So the runner never reports an unreachable dependency as a test failure.
  The same count serves `long`, `turbomode`, `short`, `medium` and
  `container-waves`; each used to keep its own.

### Policies (defaults)

| Policy | Attempts | First wait → cap | Deadline | Used for |
|---|---|---|---|---|
| `API` | 5 | 0.5 s → 8 s | 2 min | apiserver and stormblock calls (each attempt also has the client timeout: 60 s apiserver, 20 s stormblock) |
| `GATHER` | 3 | 0.5 s → 2 s | 30 s | must-gather's host collector on a node that may be sick; once a service gave up, its other endpoints get one try each (`Policy::once`) |
| `CONNECT` | 4 | 1 s → 5 s | 90 s | ssh connect + login (the caller's own timeout caps it too) |
| `PROBE` | 3 | 1 s → 2 s | 60 s | TCP, ICMP and DNS reachability probes |

Unit tests (`cargo test -p stormcos-qa-retry`): a fake that fails N times
then succeeds; giving up after the attempts as `Infra`; a real answer not
retried; the deadline capping retries; `Retry-After` honoured; a probe
stopping at its first answer; the delay schedule; and, against a local
HTTP server, a GET riding out 503/500, a 404 returned at once, a POST
repeated after 429/503 but not after a 500, and a refused connection given
up on as infrastructure. `report.rs` tests that an `Infra` failure counts
as "could not run" (exit 2), not "failed".

## Creates: check-then-act

`kube::Client::post` (qa-test) and `api::Api::send(POST)` (must-gather)
are creates. One that certainly did not land is sent again. After a
failure that may have landed, they read the object by its name: if it
carries every label that was sent, the create is done. If it is absent
(404), the create is sent again. Anything else is the original error. A
body with no labels cannot be told apart from a namesake, so it is not
taken over.

## Call sites

`file:line` as of this change.

| Call site | What it calls | Before | Now |
|---|---|---|---|
| `crates/qa-test/src/kube.rs:91` `get` | apiserver GET (every suite) | one try; a 5xx came back as an answer | `API`, Safe |
| `crates/qa-test/src/kube.rs:99` `post` | apiserver create (medium, long, containers, short, podnet, mustgather) | one try | `API`, Unsafe + check-then-act |
| `crates/qa-test/src/kube.rs:121` `post_once` | turbomode's Pod/PVC creates (`turbomode.rs` `create`) | one try | Unsafe, no check-then-act. **Not retried, because** turbomode measures its creates, and a lost acknowledgement fails the whole attempt as transient. That attempt is retried after verified cleanup (#26, owner's design; covered by `turbomode_fake` "lost claim ack") |
| `crates/qa-test/src/kube.rs:127` `put` | VM `restart` subresource (`wave.rs`) | one try | `API`, Unsafe. **Not retried after it may have landed, because** a second restart restarts the VM twice |
| `crates/qa-test/src/kube.rs:132,138` `delete`, `delete_uid` | apiserver DELETE (cleanup everywhere) | one try | `API`, Safe (a repeat finds it gone or going; `delete_uid`'s UID precondition still guards a namesake) |
| `crates/qa-test/src/kube.rs:144` `text` | pod logs | one try | `API`, Safe |
| `crates/qa-test/src/kube.rs:151` `watch` | opening a watch (turbomode's observer) | one try | `API`, Safe; a watch that ends is resumed from its resourceVersion by the observer, as before |
| `crates/qa-test/src/census.rs:117` `volume_named` | stormblock volume list (`short` golden, `long` preflight) | one try | `API`, Safe; an unreachable stormblock reads "could not run" |
| `crates/qa-test/src/census.rs:158` `Sources::json` | stormblock / stormvm reads for the residue census | one try | `API`, Safe |
| `crates/qa-test/src/turbo_audit.rs:97` | stormblock inventory for turbomode's audit | one try | `API`, Safe |
| `crates/qa-test/src/ssh.rs:76` | ssh connect + login (wave install/restart, medium's agent into VMs) | one try | `CONNECT`; a key the guest refuses is a real answer; **the command is never re-run, because** it may have acted (`dnf install`, the probe script) |
| `crates/qa-test/src/agent.rs:109` `tcp` | the agent pod's TCP probes into members and outside targets | one try, sequential | `PROBE` on no answer (`timeout`/`error`), concurrent; `attempts` in each probe record |
| `crates/qa-test/src/agent.rs` `vm_script` | ping + `/dev/tcp` from each VM | one try | 3 tries 1 s then 2 s apart in the script; `name proto result attempts` lines |
| `crates/qa-test/src/medium.rs:547` `inbound` | the driver's TCP probes from another namespace | one try, sequential | `PROBE`, concurrent; `attempts` recorded |
| `crates/qa-test/src/short.rs:410` `ssh_banner` | node :22 banner | one try | `PROBE` |
| `crates/qa-test/src/containers.rs:497` | the wave Service's ClusterIP | one try | `PROBE` |
| `crates/qa-test/src/podnet.rs:148` `answer` | `/test resolve`: Service by name and by ClusterIP | one try | `PROBE`; the detail says `(after n tries)` |
| `crates/qa-test/src/podnet.rs:173` | `/test resolve`: DNS lookup | one try | `PROBE` |
| `crates/must-gather/src/api.rs:95` `get` (and `list` paging) | apiserver GET | one try | `API`, Safe |
| `crates/must-gather/src/api.rs:102` `send` | DELETE (Safe); POST namespace / host pod (Unsafe + check-then-act: both carry labels) | one try | `API` |
| `crates/must-gather/src/host.rs:399` | stormblock endpoints from the host collector | one try | `GATHER`, then one try each once stormblock gave up |
| `crates/must-gather/src/host.rs:422` | fastetcd metrics | one try | `GATHER` |
| `gather/{ironprom,stormblock-csi,cadvisor}/status.sh` | curl GETs | one try | `curl --retry 2 --retry-delay 1 --retry-max-time 30 --retry-connrefused` (timeouts, refused, 408/429/5xx). cadvisor's busybox `wget` fallback has no retry flags: one try |

### Waits, not retries

These loops poll for a state until the step's deadline. They are bounded
waits for something to become true, not retries of a failed call. Each
look goes through the client above, so a look that fails is already
retried.

- `medium.rs` `wait_vmi`, `wait_pod`, `run_agent`; `wave.rs` `wait_up`.
  If the last look failed, they now return that error, so an unreachable
  apiserver reads "could not run" and not "not Running in time"
  (`kube::looked` / `kube::timed_out`).
- `wave.rs` `wait_ssh`, `wait_rdp`: a booting VM's sshd and the stormrdp
  gateway, polled every 3 s until the deadline.
- `short.rs:308` helper pod and `containers.rs` readiness, restart and
  reschedule loops; `must-gather` `host.rs` `collect`.

### Not remote

`mustgather.rs` runs the local `/must-gather` binary, and `must-gather`'s
`main.rs` runs the local `gather/` scripts (their own fetches are listed
above). `crash.rs`, `sqlite.rs`, `claim.rs` and `turbo_audit.rs`'s `/proc`
and cgroup reads are local files.
