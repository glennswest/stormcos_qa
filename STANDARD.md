# stormcos QA test standard (v1)

How to write a test that the stormcos builder runs against every boot image and
cluster. Tests are plain executables; the standard is the contract around them.

## Where tests live

```
tests/
  <owner>/          # owner = a repo short-name (stormblock, rustkube, …)
    <name>          # one executable test per file
  overall/          # cross-cutting tests; any project may contribute
```

- A test under `tests/stormblock/` is **owned by** `glennswest/stormblock` by
  default — that's where a failure files its issue.
- Tests under `tests/overall/` are cross-cutting, so each **must** declare its
  owner explicitly (`QA-Owner`), because "overall" is not a repo.
- A project owns its own tests. stormcos writes tests too (`tests/stormcos/`).

## A test is an executable

Any language. It must be `chmod +x`. Shell and Python are typical; a compiled
binary is fine. The runner executes it once and reads its **exit code**:

- **exit 0 → pass**
- **non-zero → fail**

stdout+stderr are captured as the test's log (attached to the issue on failure).

## Metadata

Declare metadata as `# QA-<Key>: <value>` comment lines anywhere in the file
(the runner greps them; `#` works for shell/python/most). For languages without
`#` comments, add a sibling `<name>.qa.toml` with the same keys.

| Key | Default | Meaning |
|---|---|---|
| `QA-Name` | filename | Human name. |
| `QA-Owner` | top dir → `glennswest/<dir>` | Repo to file the issue in. **Required** in `overall/`. |
| `QA-Desc` | — | One line: what it checks. |
| `QA-Scope` | `cluster` | `image` (static, on the image file) · `cluster` (on a live node) · `component` (self-contained). |
| `QA-Severity` | `blocking` | `blocking` → a failure **tombstones the image**. `warn` → files an issue but the image still ships. |
| `QA-Timeout` | `300` | Seconds before the runner kills + fails the test. |

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
| `QA_ARTIFACTS` | all | a dir to drop logs/artifacts into |
| `QA_IMAGE` | image | path to the built image file |
| `QA_NODE_IP`, `QA_NODE_NAME` | cluster | the throwaway test node |
| `QA_SSH` | cluster | an ssh command prefix, e.g. `ssh -o … root@<ip>` — run remote commands as `$QA_SSH "<cmd>"` |
| `QA_API` | cluster | the node's kube API base URL |

`image`-scope tests need no cluster and run right after a build. `cluster`-scope
tests run against a throwaway single-node cluster the builder provisions for the
QA pass (and tears down after).

Write tests to be **idempotent and self-cleaning** — no lasting mutation of the
node beyond `$QA_ARTIFACTS`.

## Failure → issue (automatic)

On a failing test the runner files a GitHub issue in `QA-Owner`, titled
`QA failure: <name>`, containing the release id, the captured log, and a hidden
marker `<!-- qa:<owner>/<name> -->`. Re-runs are **deduplicated** by the marker:
an already-open issue gets a new comment instead of a duplicate. Fixing the test
(a later pass) auto-closes it.

## Tombstoning

After a build the builder runs the QA pass. If any **blocking** test fails, the
release is **tombstoned**: it is not offered for download or provisioning and is
marked failed in the UI (with the failing tests). `warn` failures still file
issues but the image ships.

## Adding tests to your project

Put executables under `tests/<your-repo>/`, follow the metadata + exit-code
contract, and they run automatically on every build. See `tests/*/` for
examples and `crates/qa-runner` for the runner.
