#!/usr/bin/env python3
"""Explicitly invoked cluster stress test, never part of a default QA run.

Use a kubectl proxy started with an explicitly selected context. Storage runs
require a backend audit executable; API object disappearance alone is not proof
of reclamation. See README.md for the audit contract.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
from pathlib import Path
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


LABEL = "qa.storm.io/turbomode-run"


class API:
    def __init__(self, base):
        self.base = base.rstrip("/")

    def request(self, path, method="GET", body=None, raw=False):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=40) as response:
            value = response.read().decode()
            return value if raw else json.loads(value)

    def listing(self, path, selector=None):
        items, token, revision, seen = [], "", None, set()
        while True:
            query = {"limit": "500"}
            if selector:
                query["labelSelector"] = selector
            if token:
                query["continue"] = token
            value = self.request(path + "?" + urllib.parse.urlencode(query))
            rv = value["metadata"]["resourceVersion"]
            if revision is not None and revision != rv:
                raise RuntimeError("paginated LIST changed resourceVersion")
            revision = rv
            items.extend(value["items"])
            token = value["metadata"].get("continue", "")
            if not token:
                return items, revision
            if token in seen:
                raise RuntimeError("paginated LIST repeated continue token")
            seen.add(token)

    def delete(self, path, obj):
        try:
            self.request(path, "DELETE", {"apiVersion": "v1", "kind": "DeleteOptions",
                         "preconditions": {"uid": obj["metadata"]["uid"]}})
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise


class Observer:
    def __init__(self, api, selector):
        self.api, self.selector = api, selector
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.changed = threading.Event()
        self.records, self.objects, self.errors = {}, {}, []
        self.peak = 0

    def observe(self, pod, deleted=False):
        now = time.monotonic()
        uid = pod["metadata"]["uid"]
        with self.lock:
            self.objects[uid] = pod
            record = self.records.setdefault(uid, {})
            if pod.get("spec", {}).get("nodeName"):
                record.setdefault("scheduled", now)
            phase = pod.get("status", {}).get("phase")
            if phase == "Running":
                record.setdefault("running", now)
            if phase in ("Succeeded", "Failed"):
                record.setdefault("finished", now)
            if deleted:
                record["deleted"] = now
            running = sum(p.get("status", {}).get("phase") == "Running"
                          and "deleted" not in self.records[u] for u, p in self.objects.items())
            self.peak = max(self.peak, running)
        self.changed.set()

    def run(self, revision):
        while not self.stop.is_set():
            try:
                query = urllib.parse.urlencode({"watch": "true", "timeoutSeconds": 20,
                    "allowWatchBookmarks": "true", "resourceVersion": revision,
                    "labelSelector": self.selector})
                with urllib.request.urlopen(self.api.base + "/api/v1/pods?" + query, timeout=30) as response:
                    for line in response:
                        if self.stop.is_set():
                            return
                        event = json.loads(line)
                        obj = event["object"]
                        if event["type"] == "ERROR":
                            raise RuntimeError(f"watch ERROR: {obj}")
                        revision = obj["metadata"].get("resourceVersion", revision)
                        if event["type"] != "BOOKMARK":
                            self.observe(obj, event["type"] == "DELETED")
            except Exception as error:
                self.errors.append(str(error))
                # A relist recovers correctness, but a gap invalidates latency claims.
                try:
                    pods, revision = self.api.listing("/api/v1/pods", self.selector)
                    for pod in pods:
                        self.observe(pod)
                except Exception as relist_error:
                    self.errors.append(str(relist_error))
                self.stop.wait(0.2)


def percentiles(values):
    values = sorted(values)
    if not values:
        return {"samples": 0}
    return {"samples": len(values), **{f"p{p}": values[max(0, math.ceil(len(values) * p / 100) - 1)]
            for p in (50, 95, 99)}, "max": values[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", choices=("sleep", "sqlite"))
    parser.add_argument("--api", required=True, help="authenticated API or local kubectl proxy")
    parser.add_argument("--cluster", required=True, help="cluster identity recorded in evidence")
    parser.add_argument("--image", required=True, help="sleep-capable or Python+sqlite3 image")
    parser.add_argument("--storage-class")
    parser.add_argument("--claim-size", default="64Mi")
    parser.add_argument("--storage-audit", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--cleanup-timeout", type=int, default=600)
    args = parser.parse_args()
    if args.profile == "sqlite" and (not args.storage_class or not args.storage_audit):
        parser.error("sqlite requires --storage-class and --storage-audit (backend reclamation proof)")
    args.out.mkdir(parents=True, exist_ok=False)
    api = API(args.api)
    run = "turbo-" + uuid.uuid4().hex[:12]
    selector = LABEL + "=" + run
    namespaces, claims, created, pvs, errors = [], [], [], {}, []
    lock = threading.Lock()
    report = {"run": run, "profile": args.profile, "cluster": args.cluster,
              "image": args.image, "errors": errors, "sleep_seconds": 120}
    expected = 1000 if args.profile == "sleep" else 100
    report["expected_pods"] = expected
    observer = Observer(api, selector)
    thread = None

    def save():
        report.update(namespaces=namespaces, claims=claims, pods=created, pvs=list(pvs.values()))
        (args.out / "report.json").write_text(json.dumps(report, indent=2))

    def audit(phase):
        save()
        result = subprocess.run([str(args.storage_audit.resolve()), phase,
                                 str((args.out / "report.json").resolve())],
                                capture_output=True, text=True, timeout=120, check=True)
        value = json.loads(result.stdout)
        (args.out / f"storage-{phase}.json").write_text(json.dumps(value, indent=2))
        if value.get("verified") is not True:
            raise RuntimeError(f"backend {phase} audit did not verify storage")
        return value

    def capture_pvs():
        volumes, _ = api.listing("/api/v1/persistentvolumes")
        namespace_names = {n["metadata"]["name"] for n in namespaces}
        for pv in volumes:
            if pv.get("spec", {}).get("claimRef", {}).get("namespace") in namespace_names:
                pvs[pv["metadata"]["uid"]] = pv

    try:
        report["nodes"] = api.listing("/api/v1/nodes")[0]
        if args.profile == "sqlite":
            sc = api.request("/apis/storage.k8s.io/v1/storageclasses/" + args.storage_class)
            if sc.get("reclaimPolicy", "Delete") != "Delete":
                raise RuntimeError("test requires a Delete reclaim policy; shared class will not be changed")
            report["storage_class"] = sc
            audit("before")
        for i in range(100):
            name = f"{run}-{i:03}"
            ns = api.request("/api/v1/namespaces", "POST", {"apiVersion": "v1", "kind": "Namespace",
                             "metadata": {"name": name, "labels": {LABEL: run}}})
            namespaces.append(ns)
            save()
        _, revision = api.listing("/api/v1/pods", selector)
        thread = threading.Thread(target=observer.run, args=(revision,), daemon=True)
        thread.start()
        payload = Path(__file__).with_name("sqlite-workload.py").read_text()

        def create(pair):
            ns, index = pair
            name = f"pod-{index:02}"
            base = "/api/v1/namespaces/" + ns
            metadata = {"name": name, "namespace": ns, "labels": {LABEL: run}}
            container = {"name": "work", "image": args.image, "imagePullPolicy": "IfNotPresent",
                         "command": ["sleep", "120"]}
            spec = {"restartPolicy": "Never", "automountServiceAccountToken": False,
                    "containers": [container]}
            claim = None
            issued = time.monotonic()
            if args.profile == "sqlite":
                claim = api.request(base + "/persistentvolumeclaims", "POST", {
                    "apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": metadata,
                    "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": args.storage_class,
                             "resources": {"requests": {"storage": args.claim_size}}}})
                with lock:
                    claims.append(claim)
                container.update(command=["python3", "-u", "-c", payload],
                    env=[{"name": "POD_UID", "valueFrom": {"fieldRef": {"fieldPath": "metadata.uid"}}}],
                    volumeMounts=[{"name": "data", "mountPath": "/data"}])
                spec["volumes"] = [{"name": "data", "persistentVolumeClaim": {"claimName": name}}]
            pod_issued = time.monotonic()
            pod = api.request(base + "/pods", "POST", {"apiVersion": "v1", "kind": "Pod",
                                                       "metadata": metadata, "spec": spec})
            with lock:
                created.append({"object": pod, "issued": pod_issued,
                                "claim_issued": issued if claim else None, "ack": time.monotonic()})

        pairs = [(ns["metadata"]["name"], i) for ns in namespaces
                 for i in range(10 if args.profile == "sleep" else 1)]
        with ThreadPoolExecutor(max_workers=32) as pool:
            futures = [pool.submit(create, pair) for pair in pairs]
            for future in futures:
                try:
                    future.result()
                except Exception as error:
                    errors.append("create: " + str(error))
        save()
        if errors:
            raise RuntimeError("one or more creates failed")
        deadline = time.monotonic() + args.timeout
        while True:
            with observer.lock:
                objects = list(observer.objects.values())
            finished = [p for p in objects if p.get("status", {}).get("phase") in ("Succeeded", "Failed")]
            if len(finished) == expected:
                break
            if time.monotonic() >= deadline:
                raise TimeoutError(f"only {len(finished)}/{expected} Pods finished")
            observer.changed.wait(5)
            observer.changed.clear()
        for pod in finished:
            if pod["status"]["phase"] != "Succeeded":
                errors.append("Pod failed: " + pod["metadata"]["uid"])
            if args.profile == "sqlite":
                m = pod["metadata"]
                log = api.request(f"/api/v1/namespaces/{m['namespace']}/pods/{m['name']}/log", raw=True)
                (args.out / f"{m['uid']}.log").write_text(log)
                entries = [json.loads(line) for line in log.splitlines()]
                verified = next(e for e in entries if e.get("phase") == "verified")
                complete = next(e for e in entries if e.get("phase") == "complete")
                if (complete["pod_uid"] != m["uid"] or verified["pod_uid"] != m["uid"]
                        or complete["records"] != 1000 or verified["records"] != 1000
                        or complete["sha256"] != verified["sha256"] or complete["sleep_seconds"] < 120):
                    raise RuntimeError("invalid SQLite integrity evidence")
        if args.profile == "sqlite":
            capture_pvs()
            if len(pvs) != 100:
                raise RuntimeError(f"expected 100 distinct PVs, found {len(pvs)}")
            audit("allocated")
    except BaseException as error:
        errors.append(type(error).__name__ + ": " + str(error))
    finally:
        # A POST can commit even if its response is lost. Recover this run's
        # inventory by its unique label before cleanup, rather than leaking it.
        try:
            observed_ns = api.listing("/api/v1/namespaces", selector)[0]
            known = {n["metadata"]["uid"] for n in namespaces}
            namespaces.extend(n for n in observed_ns if n["metadata"]["uid"] not in known)
            observed_claims = api.listing("/api/v1/persistentvolumeclaims", selector)[0]
            known = {c["metadata"]["uid"] for c in claims}
            claims.extend(c for c in observed_claims if c["metadata"]["uid"] not in known)
        except Exception as error:
            errors.append("cleanup inventory: " + str(error))
        if thread:
            observer.stop.set()
            thread.join(timeout=35)
        report["watch_errors"] = observer.errors
        report["latency_valid"] = not observer.errors
        report["peak_running_observed"] = observer.peak
        report["observations"] = observer.records
        report["final_pods"] = list(observer.objects.values())
        report["seconds"] = {"create_ack": percentiles([p["ack"] - p["issued"] for p in created])}
        for phase in ("scheduled", "running", "finished"):
            report["seconds"]["request_to_" + phase] = percentiles([
                observer.records[p["object"]["metadata"]["uid"]][phase] - p["issued"]
                for p in created if phase in observer.records.get(p["object"]["metadata"]["uid"], {})])
        report["seconds"]["claim_request_to_running"] = percentiles([
            observer.records[p["object"]["metadata"]["uid"]]["running"] - p["claim_issued"]
            for p in created if p["claim_issued"] is not None
            and "running" in observer.records.get(p["object"]["metadata"]["uid"], {})])
        try:
            capture_pvs()
        except Exception as error:
            errors.append("PV inventory: " + str(error))
        save()
        # Delete exact run namespaces with UID preconditions. Never force finalizers
        # or directly delete PVs: that could conceal broken storage reclamation.
        cleanup_start = time.monotonic()
        for ns in namespaces:
            try:
                api.delete("/api/v1/namespaces/" + ns["metadata"]["name"], ns)
            except Exception as error:
                errors.append("namespace delete: " + str(error))
        try:
            while True:
                remaining_ns = api.listing("/api/v1/namespaces", selector)[0]
                remaining_pods = api.listing("/api/v1/pods", selector)[0]
                remaining_claims = api.listing("/api/v1/persistentvolumeclaims", selector)[0]
                all_pvs = api.listing("/api/v1/persistentvolumes")[0]
                owned_names = {n["metadata"]["name"] for n in namespaces}
                remaining_pvs = [v for v in all_pvs if v.get("spec", {}).get("claimRef", {}).get("namespace") in owned_names]
                for pv in remaining_pvs:
                    pvs[pv["metadata"]["uid"]] = pv
                pv_names = {v["metadata"]["name"] for v in pvs.values()}
                attachments = api.listing("/apis/storage.k8s.io/v1/volumeattachments")[0]
                remaining_attachments = [a for a in attachments if a.get("spec", {}).get("source", {}).get("persistentVolumeName") in pv_names]
                residue = dict(namespaces=remaining_ns, pods=remaining_pods, claims=remaining_claims,
                               pvs=remaining_pvs, attachments=remaining_attachments)
                report["residue"] = residue
                if not any(residue.values()):
                    break
                if time.monotonic() - cleanup_start >= args.cleanup_timeout:
                    raise TimeoutError("cleanup left resources; see residue")
                time.sleep(1)  # Bounded observation of pending cleanup, not a reconciler.
            report["cleanup_seconds"] = time.monotonic() - cleanup_start
            if args.profile == "sqlite":
                while True:
                    try:
                        audit("after")
                        break
                    except Exception:
                        if time.monotonic() - cleanup_start >= args.cleanup_timeout:
                            raise
                        time.sleep(1)
            report["cleanup_seconds"] = time.monotonic() - cleanup_start
            report["cleanup_verified"] = True
        except Exception as error:
            errors.append("cleanup: " + str(error))
            report["cleanup_verified"] = False
        report["passed"] = not errors and len(created) == expected and report.get("cleanup_verified", False)
        save()
    print(json.dumps({"run": run, "passed": report["passed"], "errors": errors,
                      "report": str(args.out / "report.json")}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
