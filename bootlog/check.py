#!/usr/bin/env python3
"""Boot-console review (stormcos#545, stormcos_qa#67): every line of a boot
console capture that matches the finding pattern must be explained by an
entry in known.toml (pattern -> open issue -> owner). Prints the counts per
file and every unexplained line; exits 1 when any line is unexplained.

    python3 bootlog/check.py <console.log> [...]       # counts + unexplained
    python3 bootlog/check.py --all <console.log>       # also every explained line
    python3 bootlog/check.py --before <log>            # as if nothing were known

Console captures are stormcentral's serial logs
(/var/lib/stormcentral/console/<machine>-<date>-<time>.log): raw terminal
bytes, so escape sequences and carriage returns are stripped first.
"""
import argparse
import collections
import os
import re
import sys
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))


def load(path):
    with open(path, "rb") as f:
        k = tomllib.load(f)
    k.setdefault("match", {}).setdefault("pattern", DEFAULT_MATCH)
    k.setdefault("not_a_finding", [])
    k.setdefault("known", [])
    for e in k["not_a_finding"] + k["known"]:
        e["re"] = re.compile(e["pattern"])
    return k


DEFAULT_MATCH = r"(?i:error|warn|fail|taint|timed out|denied)|Call Trace|BUG"


def clean(raw):
    """Terminal bytes to plain lines: escape sequences, CR and control bytes gone."""
    s = raw.decode("utf-8", "replace")
    s = re.sub(r"\x1b\[[0-9;=?]*[A-Za-z]", "", s)
    s = re.sub(r"\x1b[()][0-9A-B]", "", s)
    s = s.replace("\r\n", "\n").replace("\r", "\n")
    s = re.sub(r"[\x00-\x08\x0b-\x1f\x7f]", "", s)
    return s.split("\n")


def classify(lines, k):
    """Yield (line number, line, entry or None) for every finding line."""
    match = re.compile(k["match"]["pattern"])
    for i, line in enumerate(lines, 1):
        rest = line
        for n in k["not_a_finding"]:
            rest = n["re"].sub("", rest)
        if not match.search(rest):
            continue
        yield i, line, next((e for e in k["known"] if e["re"].search(line)), None)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("logs", nargs="+")
    ap.add_argument("--known", default=os.path.join(HERE, "known.toml"), help="the known-lines file")
    ap.add_argument("--before", action="store_true", help="ignore the known entries: every finding is unexplained")
    ap.add_argument("--all", action="store_true", help="list explained lines too")
    a = ap.parse_args(argv)
    k = load(a.known)
    if a.before:
        k["known"] = []
    unexplained_total = 0
    for path in a.logs:
        with open(path, "rb") as f:
            lines = clean(f.read())
        found = list(classify(lines, k))
        bad = [(i, l) for i, l, e in found if e is None]
        by_issue = collections.Counter(e["issue"] for _, _, e in found if e is not None)
        unexplained_total += len(bad)
        print(f"{path}: {len(found)} finding line(s), {len(found) - len(bad)} explained, {len(bad)} unexplained")
        for issue, n in sorted(by_issue.items(), key=lambda x: -x[1]):
            print(f"  {n:6d}  {issue}")
        for i, l in bad:
            print(f"  UNEXPLAINED {path}:{i}: {l.strip()[:300]}")
        if a.all:
            for i, l, e in found:
                if e is not None:
                    print(f"  {e['issue']:28s} {path}:{i}: {l.strip()[:200]}")
    return 1 if unexplained_total else 0


if __name__ == "__main__":
    sys.exit(main())
