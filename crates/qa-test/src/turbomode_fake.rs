//! `/test turbomode`'s driver end to end against an in-process fake
//! apiserver and stormblock (the Rust counterpart of
//! `tools/turbomode/selftest.py`): a clean run of both profiles, and the
//! failure paths that decide retries — a lost create acknowledgement
//! (transient: cleaned up by label, retried), corruption (integrity: final,
//! log kept), a volume left behind (cleanup not verified: final), and a
//! missing node name (could not run).
//!
//! The fake's Pods finish the moment they are created; its claims bind at
//! once to a PV and a stormblock volume, which deleting the claim reclaims.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::report::Out;
use crate::turbomode;

const NS: &str = "qa";

#[derive(Default, Clone, Copy)]
struct Faults {
    lose_claim_ack: bool,
    corrupt_one: bool,
    leak_volume: bool,
}

#[derive(Default)]
struct Fake {
    faults: Faults,
    lost: bool,
    corrupted: bool,
    next: u64,
    pods: BTreeMap<String, Value>,
    claims: BTreeMap<String, Value>,
    pvs: BTreeMap<String, Value>,
    volumes: BTreeMap<String, Value>,
}

fn list(items: Vec<Value>) -> String {
    json!({"metadata": {"resourceVersion": "1"}, "items": items}).to_string()
}

fn selected<'a>(objs: impl Iterator<Item = &'a Value>, query: &str) -> Vec<Value> {
    let sel = query.split('&').find_map(|kv| kv.strip_prefix("labelSelector=")).map(|s| s.replace("%3D", "=").replace("%2F", "/"));
    objs.filter(|o| {
        sel.as_ref().is_none_or(|s| {
            s.split(',').all(|kv| kv.split_once('=').is_some_and(|(k, v)| o["metadata"]["labels"][k] == v))
        })
    })
    .cloned()
    .collect()
}

impl Fake {
    fn uid(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("{kind}-uid-{}", self.next)
    }

    fn handle(&mut self, method: &str, target: &str, body: &[u8]) -> (u16, String) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let pods = format!("/api/v1/namespaces/{NS}/pods");
        let pvcs = format!("/api/v1/namespaces/{NS}/persistentvolumeclaims");
        let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let precondition = body["preconditions"]["uid"].as_str().map(str::to_string);
        match method {
            "GET" if path == "/api/v1/nodes" => (200, list(vec![json!({"metadata": {"name": "n1"}})])),
            "GET" if path == "/api/v1/persistentvolumes" => (200, list(self.pvs.values().cloned().collect())),
            "GET" if path == "/apis/storage.k8s.io/v1/volumeattachments" => (200, list(vec![])),
            "GET" if path == "/apis/storage.k8s.io/v1/storageclasses" => (200, list(vec![json!({
                "metadata": {"name": "stormblock", "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}},
                "reclaimPolicy": "Delete"})])),
            "GET" if path == pods => (200, list(selected(self.pods.values(), query))),
            "GET" if path == pvcs => (200, list(selected(self.claims.values(), query))),
            "GET" if path.starts_with(&pods) && path.ends_with("/log") => {
                let name = path.trim_start_matches(&format!("{pods}/")).trim_end_matches("/log");
                let uid = self.pods[name]["metadata"]["uid"].as_str().unwrap().to_string();
                let mut after = "sum";
                if self.faults.corrupt_one && !self.corrupted {
                    self.corrupted = true;
                    after = "changed";
                }
                let v = json!({"phase": "verified", "pod_uid": uid, "records": 1000, "sha256": "sum"});
                let c = json!({"phase": "complete", "pod_uid": uid, "records": 1000, "sha256": after, "sleep_seconds": 0.5});
                (200, format!("{v}\n{c}\n"))
            }
            "POST" if path == pvcs => {
                let name = body["metadata"]["name"].as_str().unwrap().to_string();
                let mut claim = body.clone();
                claim["metadata"]["uid"] = json!(self.uid("claim"));
                claim["metadata"]["namespace"] = json!(NS);
                self.pvs.insert(name.clone(), json!({
                    "metadata": {"name": format!("pv-{name}"), "uid": format!("pv-uid-{name}")},
                    "spec": {"claimRef": {"namespace": NS, "name": name, "uid": claim["metadata"]["uid"]},
                             "csi": {"volumeHandle": format!("vol-{name}")}}}));
                self.volumes.insert(name.clone(), json!({"name": format!("pvc-{NS}-{name}"), "id": format!("vol-{name}"), "allocated_bytes": 4096}));
                self.claims.insert(name, claim.clone());
                if self.faults.lose_claim_ack && !self.lost {
                    self.lost = true;
                    return (500, "connection reset (the claim was stored)".into());
                }
                (201, claim.to_string())
            }
            "POST" if path == pods => {
                let name = body["metadata"]["name"].as_str().unwrap().to_string();
                let mut pod = body.clone();
                pod["metadata"]["uid"] = json!(self.uid("pod"));
                pod["metadata"]["namespace"] = json!(NS);
                pod["spec"]["nodeName"] = json!("n1");
                pod["status"] = json!({"phase": "Succeeded"});
                self.pods.insert(name, pod.clone());
                (201, pod.to_string())
            }
            "DELETE" if path.starts_with(&format!("{pods}/")) => {
                let name = path.rsplit('/').next().unwrap();
                match self.pods.get(name) {
                    None => (404, "{}".into()),
                    Some(p) if precondition.as_deref().is_some_and(|u| p["metadata"]["uid"] != u) => (409, "{}".into()),
                    Some(_) => (200, self.pods.remove(name).unwrap().to_string()),
                }
            }
            "DELETE" if path.starts_with(&format!("{pvcs}/")) => {
                let name = path.rsplit('/').next().unwrap();
                match self.claims.get(name) {
                    None => (404, "{}".into()),
                    Some(c) if precondition.as_deref().is_some_and(|u| c["metadata"]["uid"] != u) => (409, "{}".into()),
                    Some(_) => {
                        self.pvs.remove(name);
                        if !self.faults.leak_volume {
                            self.volumes.remove(name);
                        }
                        (200, self.claims.remove(name).unwrap().to_string())
                    }
                }
            }
            // stormblock
            "GET" if path == "/api/v1/volumes" => {
                (200, json!({"items": self.volumes.values().collect::<Vec<_>>(), "count": self.volumes.len()}).to_string())
            }
            "GET" if path == "/api/v1/slabs" => (200, json!({"items": [], "count": 0}).to_string()),
            _ => (404, json!({"kind": "Status", "code": 404, "path": path}).to_string()),
        }
    }

    /// A watch: the matching Pods as ADDED events, then the end of the stream.
    fn watch(&self, target: &str) -> String {
        let query = target.split_once('?').map_or("", |(_, q)| q);
        selected(self.pods.values(), query).iter().map(|p| format!("{}\n", json!({"type": "ADDED", "object": p}))).collect()
    }
}

async fn conn(mut s: tokio::net::TcpStream, f: Arc<Mutex<Fake>>) -> std::io::Result<()> {
    let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
    let head_end = loop {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let mut first = head.lines().next().unwrap_or_default().split(' ');
    let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
    let (code, text) = if target.contains("watch=true") {
        tokio::time::sleep(Duration::from_millis(50)).await;
        (200, f.lock().unwrap().watch(&target))
    } else {
        f.lock().unwrap().handle(&method, &target, &buf[head_end..(head_end + len).min(buf.len())])
    };
    let resp = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    s.write_all(resp.as_bytes()).await?;
    s.shutdown().await
}

async fn serve(f: Arc<Mutex<Fake>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(conn(s, f.clone()));
        }
    });
    url
}

struct Run {
    code: i32,
    results: PathBuf,
    fake: Arc<Mutex<Fake>>,
}

impl Run {
    fn summary(&self, profile: &str) -> Value {
        let p = self.results.join("turbomode").join(profile).join("summary.json");
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
    }
    fn left(&self) -> (usize, usize, usize, usize) {
        let f = self.fake.lock().unwrap();
        (f.pods.len(), f.claims.len(), f.pvs.len(), f.volumes.len())
    }
}

async fn run(name: &str, faults: Faults, profiles: &str, attempts: u32, node: bool) -> Run {
    let root = std::env::temp_dir().join(format!("qa-turbo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let proc_root = root.join("proc");
    std::fs::create_dir_all(proc_root.join("1")).unwrap();
    std::fs::write(proc_root.join("1/mountinfo"), "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();
    std::fs::create_dir_all(root.join("cgroup/kubepods.slice")).unwrap();
    std::fs::write(root.join("token"), "t\n").unwrap();
    let fake = Arc::new(Mutex::new(Fake { faults, ..Default::default() }));
    let url = serve(fake.clone()).await;
    let results = root.join("results");
    let s = |p: PathBuf| p.display().to_string();
    let mut argv: Vec<String> = [
        "test", "--api", &url, "--token-file", "/nonexistent", "--namespace", NS, "--image", "img",
        "--results", &s(results.clone()), "--profiles", profiles, "--sleep-pods", "5", "--sqlite-pods", "3",
        "--sleep-seconds", "0", "--attempts", &attempts.to_string(), "--retry-delay", "0",
        "--finish-timeout", "10", "--cleanup-timeout", "3", "--stormblock-url", &url,
        "--proc-root", &s(proc_root), "--cgroup-root", &s(root.join("cgroup")), "--stormblock-token", &s(root.join("token")),
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    if node {
        argv.extend(["--node-name".to_string(), "n1".to_string()]);
    }
    let a = turbomode::Args::parse_from(argv);
    let code = turbomode::run(a, Out::new(&results, "turbomode")).await.unwrap();
    Run { code, results, fake }
}

#[tokio::test(flavor = "multi_thread")]
async fn both_profiles_pass_and_leave_nothing() {
    let r = run("clean", Faults::default(), "sleep,sqlite", 3, true).await;
    assert_eq!(r.code, 0);
    for p in ["sleep", "sqlite"] {
        let s = r.summary(p);
        assert_eq!(s["passed"], true, "{p}: {s}");
        assert_eq!(s["passed_on_attempt"], 1);
        assert_eq!(s["retried"], false);
    }
    let dir = r.results.join("turbomode/sqlite/attempt-1");
    for phase in ["before", "allocated", "after"] {
        let a: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("storage-{phase}.json"))).unwrap()).unwrap();
        assert_eq!(a["verified"], true, "{phase}");
    }
    let report: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["pvs"].as_array().unwrap().len(), 3);
    assert_eq!(report["seconds"]["request_to_finished"]["samples"], 3);
    assert_eq!(r.left(), (0, 0, 0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_claim_ack_is_cleaned_by_label_and_retried() {
    let r = run("lostack", Faults { lose_claim_ack: true, ..Default::default() }, "sqlite", 3, true).await;
    assert_eq!(r.code, 0);
    let s = r.summary("sqlite");
    assert_eq!(s["passed_on_attempt"], 2, "{s}");
    assert_eq!(s["retried"], true);
    let first = &s["attempts"][0];
    assert_eq!(first["cleanup_verified"], true);
    assert!(first["failures"].as_array().unwrap().iter().all(|f| f["kind"] == "transient"));
    assert_eq!(r.left(), (0, 0, 0, 0), "the claim whose ack was lost was cleaned up");
}

#[tokio::test(flavor = "multi_thread")]
async fn corruption_is_final_and_its_log_kept() {
    let r = run("corrupt", Faults { corrupt_one: true, ..Default::default() }, "sqlite", 3, true).await;
    assert_eq!(r.code, 1);
    let s = r.summary("sqlite");
    assert_eq!(s["attempts"].as_array().unwrap().len(), 1, "no retry after corruption");
    assert!(s["stopped"].as_str().unwrap().contains("integrity"), "{s}");
    let logs = std::fs::read_dir(r.results.join("turbomode/sqlite/attempt-1")).unwrap().filter(|e| {
        e.as_ref().is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".log"))
    });
    assert_eq!(logs.count(), 3);
    assert_eq!(r.left(), (0, 0, 0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leaked_volume_fails_cleanup_and_stops() {
    let r = run("leak", Faults { leak_volume: true, ..Default::default() }, "sqlite", 3, true).await;
    assert_eq!(r.code, 1);
    let s = r.summary("sqlite");
    assert_eq!(s["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(s["stopped"], "cleanup not verified");
    assert_eq!(s["attempts"][0]["cleanup_verified"], false);
    assert_eq!(r.left().3, 3, "the fake kept the volumes");
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_without_its_node_could_not_run() {
    let r = run("nonode", Faults::default(), "sqlite", 1, false).await;
    assert_eq!(r.code, 2);
    assert!(!r.results.join("turbomode/sqlite/summary.json").exists());
}
