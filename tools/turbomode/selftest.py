#!/usr/bin/env python3
"""Harness regressions; no cluster or container creation. Run on dev."""
import hashlib
import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


workload = load("sqlite-workload")
runner = load("run")


class Integrity(unittest.TestCase):
    def test_reopen_detects_modified_records_and_foreign_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.sqlite"
            db = sqlite3.connect(path)
            db.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, payload TEXT, digest TEXT)")
            with db:
                for i in range(1000):
                    data = workload.expected("first-pod", i)
                    db.execute("INSERT INTO records VALUES (?, ?, ?)",
                               (i, data, hashlib.sha256(data.encode()).hexdigest()))
            db.close()
            checksum = workload.verify(path, "first-pod")
            self.assertEqual(len(checksum), 64)
            with self.assertRaisesRegex(RuntimeError, "content mismatch"):
                workload.verify(path, "other-pod")
            db = sqlite3.connect(path)
            with db:
                db.execute("UPDATE records SET payload='corrupted' WHERE id=777")
            db.close()
            with self.assertRaisesRegex(RuntimeError, "content mismatch"):
                workload.verify(path, "first-pod")

    def test_missing_records_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.sqlite"
            db = sqlite3.connect(path)
            db.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, payload TEXT, digest TEXT)")
            db.close()
            with self.assertRaisesRegex(RuntimeError, "expected 1000 records"):
                workload.verify(path, "pod")


class Paging(unittest.TestCase):
    def api(self, pages):
        api = runner.API("http://unused")
        pages = iter(pages)
        api.request = lambda _: next(pages)
        return api

    def test_changed_revision_fails_closed(self):
        api = self.api([{"metadata": {"resourceVersion": "one", "continue": "next"}, "items": [1]},
                        {"metadata": {"resourceVersion": "two"}, "items": [2]}])
        with self.assertRaisesRegex(RuntimeError, "changed resourceVersion"):
            api.listing("/pods")

    def test_repeated_token_does_not_hang_cleanup(self):
        page = {"metadata": {"resourceVersion": "opaque", "continue": "same"}, "items": []}
        with self.assertRaisesRegex(RuntimeError, "repeated continue"):
            self.api([page, page]).listing("/pods")


if __name__ == "__main__":
    unittest.main()
