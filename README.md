# stormcos_qa

Tests, a test runner, and a must-gather for stormcos boot images and clusters.
Pulled into the builder VM; the builder runs the suite after every build and
**tombstones** any image that fails a blocking test.

## Layout

```
STANDARD.md              # the test contract — read this to write a test
tests/<owner>/           # per-project tests (tests/stormblock -> glennswest/stormblock)
tests/overall/           # cross-cutting tests (each declares QA-Owner)
gather/<area>/           # must-gather collector scripts (component-contributable)
crates/qa-runner/        # runs tests, files issues on failure, reports for tombstoning
crates/must-gather/      # debug-data collector (our oc adm must-gather), multi-node
```

## Writing a test

A test is an executable under `tests/<your-repo>/` with `# QA-*:` metadata and
an exit code (0 = pass). See **[STANDARD.md](STANDARD.md)** — that's the whole
contract. A failing test auto-files an issue in your repo (deduplicated).

## Runner

```
qa-runner --tests-dir tests --release <id> --flavor <f> \
  [--image <path>]                       # enables image-scope tests
  [--node-ip <ip> --ssh "ssh root@<ip>"] # enables cluster-scope tests
  [--file-issues] [--gather] [--report out.json]
```

Scope-gates automatically, runs each test with the standard `QA_*` env + a
timeout, files issues for failures, and — with `--gather` — runs must-gather on
failure. Exit code = number of blocking failures (the builder tombstones on
non-zero).

## must-gather (our `oc adm must-gather`)

```
must-gather --nodes 192.168.8.66[,other] --out /tmp/mg --collectors-dir gather
```

Fans out over SSH and snapshots, per node: **kernel** (dmesg, cmdline, lsmod,
io_uring/ublk state), **systemd + every component** (journal + status for
kubelet, kube-apiserver, controller-manager, scheduler, fastetcd, crio,
cadvisor, ironprom, stormblock, …), **storage** (ublk/erofs/overlay, mounts),
**network**, and the **cluster API** (nodes/pods/events) — into
`<out>/<node>/<area>/<name>.txt`, tarred with a manifest. Components extend it
by dropping a collector under `gather/<area>/` (run with the same `QA_SSH` env
as tests), so each project owns its own debug data.

## Status

Standard + runner + must-gather + example tests + example collectors build and
run. Components: add your `tests/<repo>/` and `gather/<area>/` — see the issue
filed on your repo.
