"""Tests for bootlog/check.py and known.toml: python3 -m unittest discover -s bootlog"""
import os
import re
import tempfile
import unittest

import check

KNOWN = os.path.join(check.HERE, "known.toml")


class Check(unittest.TestCase):
    def setUp(self):
        self.k = check.load(KNOWN)

    def run_on(self, text):
        return list(check.classify(check.clean(text.encode()), self.k))

    def test_escape_codes_and_carriage_returns_are_stripped(self):
        self.assertEqual(check.clean(b"\x1b[2J\x1b[01;01Hboot ok\r\nnext\r"), ["boot ok", "next", ""])

    def test_debug_and_clean_exits_are_not_findings(self):
        self.assertEqual(self.run_on("cadvisor| DEBUG sysctl set path=/proc/sys/net/ipv4/ip_forward value=1\n"
                                     "x|  INFO run closed process=stormcert-sa run_id=1 failed=false path=/a.log\n"), [])

    def test_a_kernel_bug_is_a_finding_and_case_matters(self):
        got = self.run_on("[  1.0] BUG: kernel NULL pointer dereference\n[  1.1] Call Trace:\nbugfix applied\n")
        self.assertEqual([i for i, _, _ in got], [1, 2])
        self.assertTrue(all(e is None for _, _, e in got))

    def test_known_lines_name_their_issue(self):
        got = self.run_on("stormblock| 2026-10-11T15:20:09.487291Z  WARN stormblock::mgmt::debug: api: GET /metrics 500 0ms lock-wait 0ms caller node peer 127.0.0.1:44786 req 183\n")
        self.assertEqual(got[0][2]["issue"], "stormblock#413")

    def test_a_new_failure_of_a_known_service_is_not_swallowed(self):
        got = self.run_on("cadvisor| cadvisor| Error: opening /proc/diskstats: permission denied\n"
                          "stormblock| 2026-10-11T15:20:09Z  WARN stormblock::mgmt::debug: api: GET /api/v1/volumes 500 0ms\n")
        self.assertEqual([e for _, _, e in got], [None, None])

    def test_every_entry_has_an_issue_an_owner_and_a_pattern_that_compiles(self):
        for e in self.k["known"]:
            self.assertRegex(e["issue"], r"^[a-z0-9-]+#\d+$")
            self.assertTrue(e["owner"])
            re.compile(e["pattern"])

    def test_main_exits_1_on_an_unexplained_line(self):
        with tempfile.NamedTemporaryFile("wb", suffix=".log", delete=False) as f:
            f.write(b"WARNING: unsigned - booting anyway, Secure Boot is off\r\n")
            p = f.name
        try:
            self.assertEqual(check.main([p]), 0)
            with open(p, "ab") as f:
                f.write(b"something timed out\r\n")
            self.assertEqual(check.main([p]), 1)
        finally:
            os.unlink(p)


if __name__ == "__main__":
    unittest.main()
