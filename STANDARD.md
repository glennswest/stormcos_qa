# stormcos QA test standard (v1)

How to write a test that `qa-runner` runs against a boot image and/or a live
node. A test is an ordinary executable, and this file is the contract it has to
follow.

> Nothing runs `qa-runner` automatically right now. `stormcos-builder`, which
> used to, was retired on 2026-08-23
> ([#14](https://github.com/glennswest/stormcos_qa/issues/14)). The contract
> below is what `crates/qa-runner` does today. Anything it does not do yet is
> marked **not implemented** with its issue number.

## Where tests live

```
tests/
  <owner>/          # owner = a repo short-name (stormblock, rustkube, …)
    <name>          # one executable test per file, directly in <owner>/
  overall/          # cross-cutting tests; any project may contribute
```

Only files that sit **directly** in `tests/<owner>/` are found. Tests in a
subdirectory, such as `tests/topology/single/`, are **not run yet**
([#8](https://github.com/glennswest/stormcos_qa/issues/8)). The runner skips
dotfiles, `*.qa.toml`, `README.md` and any file that is not executable.

- A test under `tests/stormblock/` is **owned by** `glennswest/stormblock` by
  default — that's where a failure files its issue.
- Tests under `tests/overall/` are cross-cutting, so each **must** declare its
  owner explicitly (`QA-Owner`), because "overall" is not a repo. An `overall/`
  test without `QA-Owner` is skipped with a message on stderr. It is not
  counted as a failure.
- A project owns its own tests. stormcos writes tests too (`tests/stormcos/`).

## A test is an executable

Any language. It must be `chmod +x`. Shell and Python are typical; a compiled
binary is fine. The runner executes it once and reads its **exit code**:

- **exit 0 → pass**
- **non-zero → fail**

stdout+stderr are captured as the test's log (attached to the issue on failure).

## Metadata

Declare metadata as `QA-<Key>: <value>` lines in the **first 40 lines** of the
file. Before matching, the runner strips any leading `#`, `/`, spaces and tabs,
so both `# QA-…` and `// QA-…` work. Keys are case-insensitive.

A sibling `<name>.qa.toml` for languages without such comments is **not
implemented**. The file is skipped and never read
([#9](https://github.com/glennswest/stormcos_qa/issues/9)).

| Key | Default | Meaning |
|---|---|---|
| `QA-Name` | filename without `.sh` | Human name. Also sets the run order and the issue title. |
| `QA-Owner` | top dir → `glennswest/<dir>` | Repo to file the issue in. **Required** in `overall/`. |
| `QA-Desc` | — | One line: what it checks. |
| `QA-Scope` | `cluster` | `image` runs only when `--image` is given. `cluster` runs only when `--ssh` is given. `component` always runs. Any other value counts as `cluster`. |
| `QA-Severity` | `blocking` | `warn` → the failure is reported but not counted as blocking. **Any other value**, a typo included, means blocking. |
| `QA-Timeout` | `300` | Seconds before the runner kills the test and marks it failed. A value that isn't a number falls back to 300. |

Example header:

```sh
#!/bin/sh
# QA-Name: ublk root device present
# QA-Owner: glennswest/stormblock
# QA-Desc: /dev/ublkb0 exists and root is erofs after boot
# QA-Scope: cluster
# QA-Severity: blocking
```

## What the runner gives a test (environment)

| Env | Scope | |
|---|---|---|
| `QA_RELEASE_ID`, `QA_FLAVOR` | all | the release under test |
| `QA_ARTIFACTS` | all | a dir to drop logs/artifacts into (runner `--artifacts`, default `/tmp/qa-artifacts`) |
| `QA_IMAGE` | image | path to the built image file |
| `QA_NODE_IP`, `QA_NODE_NAME` | cluster | the throwaway test node |
| `QA_SSH` | cluster | an ssh command prefix, e.g. `ssh -o … storm@<ip>`. Run remote commands as `$QA_SSH "<cmd>"`. On stormcos this logs in as the unprivileged `storm` user, so use `sudo -n` for anything that needs root |
| `QA_API` | all | the node's kube API base URL (runner `--api`, default `http://127.0.0.1:6443`; see [#11](https://github.com/glennswest/stormcos_qa/issues/11), because rustkube serves TLS) |

The runner always sets `QA_RELEASE_ID`, `QA_FLAVOR`, `QA_API` and
`QA_ARTIFACTS`. It sets each of the others only when the matching flag was
given.

`image`-scope tests need no cluster. `cluster`-scope tests expect a throwaway
node. Whoever runs `qa-runner` has to provision that node and tear it down
afterwards, because the runner does neither
([#2](https://github.com/glennswest/stormcos_qa/issues/2)).

Write tests to be **idempotent and self-cleaning** — no lasting mutation of the
node beyond `$QA_ARTIFACTS`.

## Failure → issue (automatic)

With `qa-runner --file-issues`, the runner uses `gh` to file a GitHub issue in
the owner repo for each failing test. The issue is titled
`QA failure: <name>`. It contains the test name, `QA-Desc`, the owner, the
release id, the scope, up to 6000 characters of log, and a hidden marker
`<!-- qa:<top-dir>/-<name-slug> -->`. The name slug is the name in lowercase
with every non-alphanumeric character turned into `-`. Re-runs are
**deduplicated** by that marker: if an issue with it is still open, it gets a
comment with the last 40 log lines instead of a new issue.

A later passing run does **not** close the issue yet
([#10](https://github.com/glennswest/stormcos_qa/issues/10)).

## Tombstoning

`qa-runner` exits with `min(blocking failures, 125)`. Its JSON report has
`"tombstone": true` when any **blocking** test failed. `warn` failures are
counted in `failed` but do not change the exit code.

Tombstoning is the **caller's** job. It means holding the release back from
download and provisioning and marking it failed. The retired
`stormcos-builder` did this. No caller does it today
([#14](https://github.com/glennswest/stormcos_qa/issues/14)).

## Adding tests to your project

Put executables under `tests/<your-repo>/`, follow the metadata + exit-code
contract, and `qa-runner` picks them up on its next run. See `tests/*/` for
examples and `crates/qa-runner` for the runner.
