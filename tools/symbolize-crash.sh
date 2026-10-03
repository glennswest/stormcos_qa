#!/usr/bin/env bash
# Name the addresses of a `/test` crash report (crates/qa-test/src/crash.rs).
#
#   sc-build 'tools/symbolize-crash.sh < tmp/crash.txt'      # from a checkout
#
# Run on the build box at the commit that crashed: it builds the binary
# again with test/build.sh (reproducible), takes the load base from
# `crash:anchor` (the runtime address of crash::install), and prints each
# `crash:rip` / `crash:ret` address with the function it falls in. The
# report is the `crash:` lines of the pod log, on stdin.
set -euo pipefail
cd "$(dirname "$0")/.."
report=$(grep -o 'crash:[a-z]*=[^ ]*' || true)
[ -n "$report" ] || { echo "no crash: lines on stdin" >&2; exit 2; }
test/build.sh >&2
nm -n -C test/out/test > test/out/test.syms
REPORT="$report" python3 - test/out/test.syms <<'PY'
import bisect, os, re, sys
syms = []
for l in open(sys.argv[1]):
    p = l.rstrip("\n").split(" ", 2)
    if len(p) == 3 and p[1] in "tTwW":
        syms.append((int(p[0], 16), p[2]))
addrs = [a for a, _ in syms]
lines = os.environ["REPORT"].split()
anchor = next(int(l.split("=")[1], 16) for l in lines if l.startswith("crash:anchor="))
static = next(a for a, n in syms if re.search(r"\bcrash::install$", n))
base = anchor - static
print(f"load base {base:#x}")
for l in lines:
    k, v = l[len("crash:"):].split("=", 1)
    if k not in ("rip", "ret"):
        print(f"{k:6} {v}")
        continue
    a = int(v, 16) - base
    i = bisect.bisect_right(addrs, a) - 1
    name = f"{syms[i][1]}+{a - syms[i][0]:#x}" if i >= 0 else "?"
    print(f"{k:6} {v}  {name}")
PY
