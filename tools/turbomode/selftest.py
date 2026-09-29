#!/usr/bin/env python3
"""Harness regressions; no cluster or container creation. Run on dev."""
import contextlib
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import sqlite3
import tempfile
import threading
import time
import unittest
import urllib.parse
import uuid


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


class FakeCluster:
    """Just enough of the Kubernetes API for run.py, with injectable faults.

    Pods finish as soon as they are created (the workload is not executed);
    a watch stream closes after a short idle so the runner can stop quickly.
    Fault budgets are consumed across attempts, so a fault can hit only the
    first attempt and the retry runs clean.
    """

    def __init__(self, lose_acks=0, stuck_pods=0, corrupt=0, fail_pods=0,
                 watch_410=0, stuck_pv=False):
        self.lose_acks, self.stuck_pods, self.corrupt = lose_acks, stuck_pods, corrupt
        self.fail_pods, self.watch_410, self.stuck_pv = fail_pods, watch_410, stuck_pv
        self.rv = 0
        self.cond = threading.Condition()
        self.events = []  # (rv, type, pod)
        self.namespaces, self.pods, self.claims, self.pvs = {}, {}, {}, {}
        self.corrupt_uids = set()
        cluster = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def body(self):
                length = int(self.headers.get("Content-Length") or 0)
                return json.loads(self.rfile.read(length)) if length else None

            def send(self, code, value, raw=False):
                data = value.encode() if raw else json.dumps(value).encode()
                self.send_response(code)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                cluster.get(self)

            def do_POST(self):
                cluster.post(self)

            def do_DELETE(self):
                cluster.delete(self)

        class Server(ThreadingHTTPServer):
            request_queue_size = 256  # the runner creates 32 at a time

        self.server = Server(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def meta(self, obj, name, namespace=None):
        self.rv += 1
        obj.setdefault("metadata", {}).update(name=name, uid=str(uuid.uuid4()),
                                              resourceVersion=str(self.rv))
        if namespace:
            obj["metadata"]["namespace"] = namespace
        return obj

    def emit(self, kind, pod):
        self.rv += 1
        pod = json.loads(json.dumps(pod))
        pod["metadata"]["resourceVersion"] = str(self.rv)
        self.events.append((self.rv, kind, pod))
        self.cond.notify_all()

    @staticmethod
    def selected(obj, query):
        selector = query.get("labelSelector", [None])[0]
        if not selector:
            return True
        key, value = selector.split("=", 1)
        return obj["metadata"].get("labels", {}).get(key) == value

    def get(self, h):
        url = urllib.parse.urlsplit(h.path)
        query = urllib.parse.parse_qs(url.query)
        parts = url.path.strip("/").split("/")
        if url.path == "/api/v1/pods" and query.get("watch"):
            return self.watch(h, int(query["resourceVersion"][0]), query)
        with self.cond:
            if url.path.endswith("/log"):
                ns, name = parts[3], parts[5]
                pod = next((p for p in self.pods.values() if p["metadata"]["namespace"] == ns
                            and p["metadata"]["name"] == name), None)
                if pod is None:
                    return h.send(404, {})
                uid = pod["metadata"]["uid"]
                good = hashlib.sha256(uid.encode()).hexdigest()
                after = "0" * 64 if uid in self.corrupt_uids else good
                lines = [{"phase": "verified", "pod_uid": uid, "records": 1000, "sha256": good},
                         {"phase": "complete", "pod_uid": uid, "records": 1000, "sha256": after,
                          "sleep_seconds": 120.0}]
                return h.send(200, "\n".join(json.dumps(line) for line in lines) + "\n", raw=True)
            if parts[-2:] == ["storageclasses", "fake"]:
                return h.send(200, {"metadata": {"name": "fake"}, "reclaimPolicy": "Delete"})
            source = {"/api/v1/nodes": {"n": {"metadata": {"name": "node-1", "uid": "n"}}},
                      "/api/v1/namespaces": self.namespaces, "/api/v1/pods": self.pods,
                      "/api/v1/persistentvolumeclaims": self.claims,
                      "/api/v1/persistentvolumes": self.pvs,
                      "/apis/storage.k8s.io/v1/volumeattachments": {}}.get(url.path)
            if source is None:
                return h.send(404, {})
            items = [o for o in source.values() if self.selected(o, query)]
            h.send(200, {"metadata": {"resourceVersion": str(self.rv)}, "items": items})

    def watch(self, h, revision, query):
        h.send_response(200)
        h.end_headers()
        with self.cond:
            if self.watch_410:
                self.watch_410 -= 1
                h.wfile.write(json.dumps({"type": "ERROR", "object": {
                    "kind": "Status", "code": 410, "reason": "Expired"}}).encode() + b"\n")
                return
        idle = time.monotonic() + 0.3
        while time.monotonic() < idle:
            with self.cond:
                pending = [e for e in self.events if e[0] > revision and self.selected(e[2], query)]
                if not pending:
                    self.cond.wait(0.1)
                    continue
            for rv, kind, pod in pending:
                h.wfile.write(json.dumps({"type": kind, "object": pod}).encode() + b"\n")
                revision = rv
            h.wfile.flush()
            idle = time.monotonic() + 0.3

    def post(self, h):
        parts = urllib.parse.urlsplit(h.path).path.strip("/").split("/")
        obj = h.body()
        with self.cond:
            name = obj["metadata"]["name"]
            if parts[-1] == "namespaces":
                self.namespaces[name] = self.meta(obj, name)
                return h.send(201, obj)
            ns = parts[3]
            if parts[-1] == "persistentvolumeclaims":
                claim = self.meta(obj, name, ns)
                self.claims[claim["metadata"]["uid"]] = claim
                pv = self.meta({"spec": {"claimRef": {"namespace": ns, "name": name},
                               "csi": {"volumeHandle": "vol-" + claim["metadata"]["uid"]}}},
                               "pvc-" + claim["metadata"]["uid"])
                self.pvs[pv["metadata"]["uid"]] = pv
                return h.send(201, claim)
            pod = self.meta(obj, name, ns)
            self.pods[pod["metadata"]["uid"]] = pod
            pod["status"] = {"phase": "Pending"}
            self.emit("ADDED", pod)
            if self.stuck_pods:
                self.stuck_pods -= 1
            else:
                pod["spec"]["nodeName"] = "node-1"
                pod["status"] = {"phase": "Running"}
                self.emit("MODIFIED", pod)
                phase = "Succeeded"
                if self.fail_pods:
                    self.fail_pods -= 1
                    phase = "Failed"
                if self.corrupt:
                    self.corrupt -= 1
                    self.corrupt_uids.add(pod["metadata"]["uid"])
                pod["status"] = {"phase": phase}
                self.emit("MODIFIED", pod)
            if self.lose_acks:
                # Committed, but the client never learns it.
                self.lose_acks -= 1
                return h.send(500, {"kind": "Status", "code": 500})
            h.send(201, pod)

    def delete(self, h):
        parts = urllib.parse.urlsplit(h.path).path.strip("/").split("/")
        body = h.body() or {}
        with self.cond:
            ns = self.namespaces.get(parts[-1])
            if ns is None:
                return h.send(404, {})
            if body.get("preconditions", {}).get("uid") not in (None, ns["metadata"]["uid"]):
                return h.send(409, {})
            del self.namespaces[parts[-1]]
            for uid, pod in list(self.pods.items()):
                if pod["metadata"]["namespace"] == parts[-1]:
                    del self.pods[uid]
                    self.emit("DELETED", pod)
            for uid, claim in list(self.claims.items()):
                if claim["metadata"]["namespace"] == parts[-1]:
                    del self.claims[uid]
            if not self.stuck_pv:
                for uid, pv in list(self.pvs.items()):
                    if pv["spec"]["claimRef"]["namespace"] == parts[-1]:
                        del self.pvs[uid]
            h.send(200, {})


AUDITOR = """#!/usr/bin/env python3
import json, pathlib, sys
phase = sys.argv[1]
control = json.loads((pathlib.Path(__file__).parent / "audit-control.json").read_text())
calls = pathlib.Path(__file__).parent / "audit-calls"
calls.open("a").write(phase + "\\n")
if phase in control.get("unreachable", []):
    sys.exit("stormblock: connection refused")
print(json.dumps({"verified": phase not in control.get("unverified", []), "phase": phase}))
"""


class Retries(unittest.TestCase):
    """Drive the real runner against FakeCluster: failure paths keep their
    evidence, and a retry never follows a leak or an integrity failure."""

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="turbo-selftest-"))
        self.auditor = self.dir / "audit.py"
        self.auditor.write_text(AUDITOR)
        self.auditor.chmod(0o755)
        self.control({})
        self.cluster = None

    def tearDown(self):
        if self.cluster:
            self.cluster.close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def control(self, value):
        (self.dir / "audit-control.json").write_text(json.dumps(value))

    def run_profile(self, profile="sqlite", attempts=3, extra=(), **faults):
        self.cluster = FakeCluster(**faults)
        out = self.dir / "out"
        argv = [profile, "--api", self.cluster.url, "--cluster", "fake", "--image", "fake",
                "--out", str(out), "--attempts", str(attempts), "--retry-delay", "0",
                "--timeout", "3", "--cleanup-timeout", "3", *extra]
        if profile == "sqlite":
            argv += ["--storage-class", "fake", "--storage-audit", str(self.auditor)]
        with contextlib.redirect_stdout(io.StringIO()):
            code = runner.main(argv)
        summary = json.loads((out / "summary.json").read_text())
        reports = [json.loads((out / a["report"]).read_text()) for a in summary["attempts"]]
        return code, summary, reports

    def kinds(self, report):
        return {f["kind"] for f in report["failures"]}

    def test_clean_run_passes_first_time(self):
        code, summary, (report,) = self.run_profile()
        self.assertEqual(code, 0, report["errors"])
        self.assertEqual(summary["passed_on_attempt"], 1)
        self.assertFalse(summary["retried"])
        self.assertTrue(report["latency_valid"])
        self.assertEqual(report["seconds"]["request_to_finished"]["samples"], 100)
        calls = (self.dir / "audit-calls").read_text().split()
        self.assertEqual(calls, ["before", "allocated", "after"])

    def test_sleep_profile_creates_all_thousand(self):
        code, summary, (report,) = self.run_profile("sleep", attempts=1)
        self.assertEqual(code, 0, report["errors"])
        self.assertEqual(len(report["pods"]), 1000)
        self.assertEqual(len(report["namespaces"]), 100)

    def test_lost_create_ack_is_cleaned_up_then_retried(self):
        code, summary, reports = self.run_profile(lose_acks=1)
        self.assertEqual(code, 0)
        self.assertEqual(summary["passed_on_attempt"], 2)
        self.assertTrue(summary["retried"])
        first = reports[0]
        self.assertFalse(first["passed"])
        self.assertTrue(first["cleanup_verified"])
        self.assertEqual(self.kinds(first), {"transient"})
        self.assertEqual(len(first["pods"]), 99)  # the lost one was never acknowledged
        self.assertFalse(self.cluster.pods)       # ... but it was deleted with its namespace
        self.assertTrue((self.dir / "out" / "attempt-1" / "report.json").exists())

    def test_partial_startup_is_retried_after_verified_cleanup(self):
        code, summary, reports = self.run_profile(stuck_pods=3)
        self.assertEqual(code, 0)
        self.assertEqual(len(reports), 2)
        self.assertIn("partial startup: only 97/100", reports[0]["errors"][-1])
        self.assertTrue(reports[0]["cleanup_verified"])

    def test_interrupted_watch_invalidates_latency_not_the_run(self):
        code, summary, (report,) = self.run_profile(watch_410=1)
        self.assertEqual(code, 0, report["errors"])
        self.assertFalse(report["latency_valid"])
        self.assertIn("410", report["watch_errors"][0])

    def test_corruption_is_final(self):
        code, summary, reports = self.run_profile(corrupt=1)
        self.assertEqual(code, 1)
        self.assertEqual(len(reports), 1, "a corrupt attempt must never be retried")
        self.assertIn("integrity", self.kinds(reports[0]))
        self.assertTrue(reports[0]["cleanup_verified"])
        self.assertIn("final failure", summary["stopped"])
        bad = next(f for f in reports[0]["failures"] if "checksum changed" in f["error"])
        uid = bad["error"].split()[2].rstrip(":")
        self.assertTrue((self.dir / "out" / "attempt-1" / f"{uid}.log").exists())

    def test_failed_sqlite_pod_is_integrity_not_transient(self):
        code, summary, reports = self.run_profile(fail_pods=1)
        self.assertEqual((code, len(reports)), (1, 1))
        self.assertIn("integrity", self.kinds(reports[0]))

    def test_cleanup_timeout_is_final_and_keeps_residue(self):
        code, summary, reports = self.run_profile(lose_acks=1, stuck_pv=True)
        self.assertEqual(code, 1)
        self.assertEqual(len(reports), 1, "a leak must stop retries")
        self.assertFalse(reports[0]["cleanup_verified"])
        self.assertEqual(len(reports[0]["residue"]["pvs"]), 100)
        self.assertEqual(summary["stopped"], "cleanup not verified")

    def test_backend_inventory_failure_after_cleanup_is_final(self):
        self.control({"unreachable": ["after"]})
        code, summary, reports = self.run_profile(lose_acks=1)
        self.assertEqual((code, len(reports)), (1, 1))
        self.assertFalse(reports[0]["cleanup_verified"])
        stderr = self.dir / "out" / "attempt-1" / "storage-after.stderr"
        self.assertIn("No route to host", stderr.read_text())

    def test_unverified_allocation_is_final(self):
        self.control({"unverified": ["allocated"]})
        code, summary, reports = self.run_profile()
        self.assertEqual((code, len(reports)), (1, 1))
        self.assertIn("storage", self.kinds(reports[0]))

    def test_retries_are_bounded(self):
        code, summary, reports = self.run_profile(attempts=2, stuck_pods=1000)
        self.assertEqual((code, len(reports)), (1, 2))
        self.assertEqual(summary["stopped"], "all 2 attempts failed")
        # Counts never shrink between attempts.
        self.assertEqual([r["expected_pods"] for r in reports], [100, 100])

    def test_attempts_outside_bounds_are_refused(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            runner.parse(["sleep", "--api", "x", "--cluster", "c", "--image", "i",
                          "--out", "o", "--attempts", str(runner.MAX_ATTEMPTS + 1)])


class Evidence(unittest.TestCase):
    def test_problems(self):
        good = [{"phase": "verified", "pod_uid": "u", "records": 1000, "sha256": "a"},
                {"phase": "complete", "pod_uid": "u", "records": 1000, "sha256": "a",
                 "sleep_seconds": 120.5}]
        log = lambda lines: "\n".join(json.dumps(line) for line in lines)
        self.assertIsNone(runner.sqlite_evidence_problem(log(good), "u"))
        self.assertIn("another Pod", runner.sqlite_evidence_problem(log(good), "v"))
        self.assertIn("missing", runner.sqlite_evidence_problem(log(good[:1]), "u"))
        self.assertIn("not JSON", runner.sqlite_evidence_problem("Traceback (most recent", "u"))
        short = [good[0], dict(good[1], sleep_seconds=3)]
        self.assertIn("less than 120", runner.sqlite_evidence_problem(log(short), "u"))

    def test_retryable_needs_clean_and_transient_only(self):
        base = {"passed": False, "cleanup_verified": True}
        self.assertTrue(runner.retryable(dict(base, failures=[{"kind": "transient"}])))
        self.assertFalse(runner.retryable(dict(base, failures=[{"kind": "transient"}, {"kind": "integrity"}])))
        self.assertFalse(runner.retryable(dict(base, cleanup_verified=False, failures=[{"kind": "transient"}])))
        self.assertFalse(runner.retryable(dict(base, failures=[])))



class FakeStormblock(BaseHTTPRequestHandler):
    """stormblock's read API: volumes, slabs and slots, token required."""
    state = {}

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("Authorization") != "Bearer node-token":
            self.send_response(401)
            self.end_headers()
            return
        path = self.path.split("/api/v1/", 1)[1]
        items = self.state.get(path, [])
        body = {"items": items, "count": len(items) + (1 if path in self.state.get("short", []) else 0)}
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class NodeAudit(unittest.TestCase):
    """The built-in auditor on the node (owner, #26): no ssh, host state from
    the Job's read-only mounts, one node only."""
    UID = "11111111-2222-3333-4444-555555555555"

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="turbo-audit-"))
        self.proc = self.dir / "proc"
        (self.proc / "1").mkdir(parents=True)
        (self.proc / "1" / "mountinfo").write_text("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n")
        (self.proc / "1" / "cgroup").write_text("0::/init.scope\n")
        (self.proc / "self").mkdir()
        self.cgroup = self.dir / "cgroup"
        (self.cgroup / "kubepods.slice").mkdir(parents=True)
        (self.dir / "token").write_text("node-token\n")
        FakeStormblock.state = {"volumes": [], "slabs": [{"id": "s1", "slot_size": 4096}], "slabs/s1/slots": []}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeStormblock)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.env = {"TURBOMODE_NODE": "node-1", "TURBOMODE_PROC": str(self.proc),
                    "TURBOMODE_CGROUP": str(self.cgroup), "TURBOMODE_TOKEN": str(self.dir / "token"),
                    "TURBOMODE_STORMBLOCK": f"http://127.0.0.1:{self.server.server_address[1]}"}
        self.report = self.dir / "report.json"
        self.write_report()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def write_report(self, nodes=("node-1",), claims=100):
        self.report.write_text(json.dumps({
            "nodes": [{"metadata": {"name": n}} for n in nodes],
            "claims": [{"metadata": {"namespace": f"t-{i}", "name": "data"}} for i in range(claims)],
            "pods": [{"object": {"metadata": {"uid": self.UID}}}], "pvs": []}))

    def audit(self, phase):
        import subprocess
        env = {k: v for k, v in os.environ.items() if not k.startswith(("TURBOMODE_", "STORM_"))}
        env.update(self.env)
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("stormblock-audit.py")),
                                 phase, str(self.report)], env=env, capture_output=True, text=True, timeout=60)
        return result.returncode, (json.loads(result.stdout) if result.returncode == 0 else result.stderr)

    def test_clean_node_verifies_after_and_records_unmeasured_pod_dirs(self):
        code, out = self.audit("after")
        self.assertEqual(code, 0, out)
        self.assertTrue(out["verified"])
        self.assertEqual(out["nodes"]["node-1"]["unmeasured"], ["pod_directories"])

    def test_allocated_needs_all_hundred_volumes(self):
        vols = [{"id": f"v{i}", "name": f"pvc-t-{i}-data", "allocated_bytes": 4096} for i in range(100)]
        FakeStormblock.state["volumes"] = vols
        self.assertTrue(self.audit("allocated")[1]["verified"])
        FakeStormblock.state["volumes"] = vols[:99]
        self.assertFalse(self.audit("allocated")[1]["verified"])

    def test_leftovers_fail_after(self):
        for leftover in ("mount", "process-cgroup", "cgroup-dir", "slot"):
            with self.subTest(leftover):
                self.setUp()
                if leftover == "mount":
                    (self.proc / "1" / "mountinfo").write_text(
                        f"9 1 0:5 / /var/lib/kubelet/pods/{self.UID}/volumes rw - ext4 /dev/sb0 rw\n")
                elif leftover == "process-cgroup":
                    (self.proc / "4242").mkdir()
                    (self.proc / "4242" / "cgroup").write_text(f"0::/kubepods/pod{self.UID}/c\n")
                elif leftover == "cgroup-dir":
                    (self.cgroup / "kubepods.slice" / f"kubepods-pod{self.UID.replace('-', '_')}.slice").mkdir()
                else:
                    FakeStormblock.state["volumes"] = [{"id": "v7", "name": "pvc-t-7-data", "allocated_bytes": 1}]
                    FakeStormblock.state["slabs/s1/slots"] = [{"index": 3, "volume_id": "v7"}]
                code, out = self.audit("after")
                self.assertEqual(code, 0, out)
                self.assertFalse(out["verified"])
                self.tearDown()
        self.setUp()

    def test_other_nodes_fail_closed(self):
        self.write_report(nodes=("node-1", "node-2"))
        code, err = self.audit("after")
        self.assertNotEqual(code, 0)
        self.assertIn("sees only node node-1", err)

    def test_missing_host_access_fails(self):
        for missing in ("token", "cgroup", "hostpid", "node"):
            with self.subTest(missing):
                env = dict(self.env)
                if missing == "token":
                    env["TURBOMODE_TOKEN"] = str(self.dir / "absent")
                elif missing == "cgroup":
                    env["TURBOMODE_CGROUP"] = str(self.dir / "absent")
                elif missing == "hostpid":
                    env["TURBOMODE_PROC"] = str(self.dir / "cgroup")
                    env["TURBOMODE_HOST_MOUNTINFO"] = str(self.proc / "1" / "mountinfo")
                else:
                    del env["TURBOMODE_NODE"]
                saved, self.env = self.env, env
                code, _ = self.audit("after")
                self.env = saved
                self.assertNotEqual(code, 0, missing)

    def test_incomplete_inventory_or_bad_token_fails(self):
        FakeStormblock.state["short"] = ["volumes"]
        self.assertNotEqual(self.audit("after")[0], 0)
        FakeStormblock.state["short"] = []
        (self.dir / "token").write_text("wrong\n")
        self.assertNotEqual(self.audit("after")[0], 0)


if __name__ == "__main__":
    unittest.main()
