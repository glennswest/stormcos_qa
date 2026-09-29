#!/usr/bin/env python3
"""Executed inside each PVC-backed container; errors must terminate the Pod."""
import hashlib
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import time


def expected(identity, record):
    # Different contents per Pod expose accidental cross-claim sharing.
    return hashlib.sha256(f"{identity}:{record}".encode()).hexdigest() * 16


def verify(path, identity):
    with closing(sqlite3.connect(f"file:{path}?mode=ro", uri=True)) as db:
        rows = db.execute("SELECT id, payload, digest FROM records ORDER BY id").fetchall()
        if len(rows) != 1000:
            raise RuntimeError(f"expected 1000 records, got {len(rows)}")
        checksum = hashlib.sha256()
        for index, (record, payload, digest) in enumerate(rows):
            if record != index or payload != expected(identity, index):
                raise RuntimeError(f"record {index} content mismatch")
            if digest != hashlib.sha256(payload.encode()).hexdigest():
                raise RuntimeError(f"record {index} checksum mismatch")
            checksum.update(payload.encode())
        integrity = db.execute("PRAGMA integrity_check").fetchall()
        if integrity != [("ok",)]:
            raise RuntimeError(f"integrity_check failed: {integrity}")
        return checksum.hexdigest()


def main():
    identity = os.environ["POD_UID"]
    path = Path("/data/records.sqlite")
    # A fresh PVC must not contain a database left by another run.
    if path.exists():
        raise RuntimeError("unexpected database on fresh PVC")
    begin = time.monotonic()
    db = sqlite3.connect(path)
    db.execute("PRAGMA journal_mode=DELETE")
    db.execute("PRAGMA synchronous=FULL")
    db.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, payload TEXT NOT NULL, digest TEXT NOT NULL)")
    with db:
        for i in range(1000):
            payload = expected(identity, i)
            db.execute("INSERT INTO records VALUES (?, ?, ?)",
                       (i, payload, hashlib.sha256(payload.encode()).hexdigest()))
    db.close()
    checksum = verify(path, identity)
    print(json.dumps({"phase": "verified", "pod_uid": identity, "records": 1000,
                      "sha256": checksum, "write_read_seconds": time.monotonic() - begin}), flush=True)
    sleep_begin = time.monotonic()
    time.sleep(120)
    after = verify(path, identity)
    if after != checksum:
        raise RuntimeError("database changed during sleep")
    print(json.dumps({"phase": "complete", "pod_uid": identity, "records": 1000,
                      "sha256": after, "sleep_seconds": time.monotonic() - sleep_begin}), flush=True)


if __name__ == "__main__":
    main()
