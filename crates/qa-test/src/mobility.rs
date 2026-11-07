//! `/test mobility` — container, VM and claim movement between two nodes
//! (#84, stormcos#806; stormcos `docs/MOBILITY.md`).
//!
//! Every mechanism it measures lands in another component, and the suite
//! goes in before them: a request nobody answers is `skip: <issue>`, never a
//! fail. A mechanism that answers and then fails is a fail, with its reason.
//!
//! What it asks for and reads (rustkube-node#338/#339's proposal; said on
//! those issues so the movers match it):
//! - request: annotations `storm.io/move-to: <node>` and/or
//!   `storm.io/move-set: <RAID set / slab>` on the PVC (claims) or the
//!   VirtualMachine (a stopped VM's disks);
//! - progress: `storm.io/move-state` on the same object: `synced` (the
//!   mirror is ready; the claim cuts over once its pod is gone), `complete`
//!   (also `completed`, `done`), `failed` / `aborted` / `refused` (with
//!   `storm.io/move-reason`), anything else is in progress;
//! - VM live migration: KubeVirt's `VirtualMachineInstanceMigration` and the
//!   VMI's `status.migrationState` (rustkube#184, rustkube-node#40).
//!
//! Nodes: two or more Ready, schedulable Nodes, else every line is a skip
//! (the runner's `two-node` cluster is stormcentral#1271). Nothing is pinned:
//! A is where the first pod or VM landed, B another Ready node, so the PV's
//! node affinity (claims) or the migration decides the second placement.
//!
//! Lines, in the issue's order:
//! - `move/claim-node` (test 1 without the drain): a `/test claim` pod writes
//!   its claim on A; the PVC asks `move-to: B`; at `synced` the pod is
//!   deleted (the eviction's stand-in) and a replacement made; pass when it
//!   is Ready on B and the sha256 of the claim's file set equals A's.
//!   Measured: eviction → Ready on B.
//! - `move/drain`: skip — the drain is a runner fault between phases
//!   (stormcentral#1161), and the drain moving claims is stormcluster#104.
//! - `move/claim-set` (test 4): `move-set: <--raid-set>` on a claim in use;
//!   pass when it completes, the bytes are equal, and the pod kept running
//!   (same uid, no restart) or the pause is stated. Measured: copy time,
//!   pause (the longest gap in TCP probes of the pod's :8080, every 200 ms).
//! - `move/claim-node-set` (test 5): both annotations in one request; as 1
//!   and 4.
//! - `move/vm-live` (test 2): a qemu VM (golden-clone root + emptyDisk, on the
//!   node bridge) runs a checksum loop; a migration moves it; pass when it
//!   runs on B with the same boot id, more uptime, the loop never failed.
//!   Measured: downtime (longest gap in probes of the guest's :22), total.
//!   A migration that fails for the kubelet's missing settings is
//!   `skip: stormcos#682`.
//! - `move/vm-cold` (test 3): a VM writes a checksummed file, is stopped,
//!   `move-to: B` on the VirtualMachine, started; pass when it boots on B
//!   with the file intact. Measured: stop → guest up on B.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;
use serde_json::{Value, json};

use crate::agent::SERVE_PORT;
use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};
use crate::ssh;

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub const MOVE_TO: &str = "storm.io/move-to";
pub const MOVE_SET: &str = "storm.io/move-set";
pub const MOVE_STATE: &str = "storm.io/move-state";
pub const MOVE_REASON: &str = "storm.io/move-reason";

/// Where each mechanism is tracked: the skip line's pointer.
const TWO_NODES: &str = "stormcentral#1271";
const CLAIM_NODE: &str = "rustkube-node#338";
const CLAIM_SET: &str = "stormblock#426, stormstorage#106";
const CLAIM_NODE_SET: &str = "rustkube-node#338, stormstorage#106";
const VM_COLD: &str = "rustkube-node#339";
const VM_LIVE: &str = "stormcos#682";
const DRAIN: &str = "stormcentral#1161 (the drain as a runner fault), stormcluster#104 (the drain moves claims)";
const NO_SET: &str = "no RAID set named to move onto (--raid-set / STORM_MOBILITY_SET); forge1's set is stormraid#49";

pub const TESTS: [&str; 6] = ["move/claim-node", "move/drain", "move/claim-set", "move/claim-node-set", "move/vm-live", "move/vm-cold"];

#[derive(Parser, Debug)]
#[command(name = "test mobility", about = "Container, VM and claim movement between two nodes (#84)")]
pub struct Args {
    #[arg(long, env = "STORM_API", default_value = "")]
    api: String,
    #[arg(long)]
    token_file: Option<String>,
    #[arg(long)]
    insecure: bool,
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    #[arg(long, env = "STORM_RUN_ID", default_value = "manual")]
    run_id: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: std::path::PathBuf,
    /// The whole run's window, seconds (the runner's budget): a test whose
    /// worst case no longer fits is a skip, so cleanup always runs.
    #[arg(long, env = "STORM_TIMEOUT", default_value_t = 1800)]
    timeout: u64,
    /// This test's image (default: the Job pod's own).
    #[arg(long)]
    image: Option<String>,
    /// The claims' StorageClass (default: the cluster's default, the built-in class).
    #[arg(long)]
    storage_class: Option<String>,
    /// The RAID set (or slab) tests 4 and 5 move a claim onto; none: they skip.
    #[arg(long, env = "STORM_MOBILITY_SET")]
    raid_set: Option<String>,
    /// How long a pod may take to be Ready with its claim's line.
    #[arg(long, default_value_t = 240)]
    pod_timeout: u64,
    /// How long a move request may go unanswered (no `storm.io/move-state`,
    /// no `migrationState`) before its mechanism counts as absent.
    #[arg(long, default_value_t = 120)]
    mechanism_wait: u64,
    /// How long an answered move may take to finish.
    #[arg(long, default_value_t = 420)]
    move_timeout: u64,
    /// How long a VM may take to be up with ssh.
    #[arg(long, default_value_t = 600)]
    vm_timeout: u64,
    /// No VM tests (2, 3): skips.
    #[arg(long)]
    no_vms: bool,
    /// The Linux golden each VM's root disk is cloned from.
    #[arg(long, default_value = "fedora-44-x86_64")]
    golden: String,
    /// The golden's cloud user.
    #[arg(long, default_value = "fedora")]
    ssh_user: String,
    /// Host bridge the VMs' NIC is put on (`storm.io/bridge`).
    #[arg(long, default_value = "stormbr0")]
    bridge: String,
    #[arg(long, default_value_t = 2048)]
    vm_memory_mib: u64,
}

// ---------------------------------------------------------------- judging

/// The Nodes a move may use: Ready and schedulable, by name.
pub fn movable_nodes(nodes: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = nodes
        .iter()
        .filter(|n| n["spec"]["unschedulable"] != true)
        .filter(|n| n["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")))
        .filter_map(|n| n["metadata"]["name"].as_str().map(str::to_string))
        .collect();
    v.sort();
    v
}

/// Another movable node than `a`.
pub fn other(nodes: &[String], a: &str) -> Option<String> {
    nodes.iter().find(|n| n.as_str() != a).cloned()
}

/// A move as its object says it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Move {
    /// No `storm.io/move-state`: nothing has taken the request.
    Unanswered,
    Progress(String),
    Synced,
    Complete,
    Failed(String),
}

pub fn move_state(obj: &Value) -> Move {
    let a = &obj["metadata"]["annotations"];
    let reason = a[MOVE_REASON].as_str().unwrap_or("no reason given");
    match a[MOVE_STATE].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        None => Move::Unanswered,
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "synced" => Move::Synced,
            "complete" | "completed" | "done" => Move::Complete,
            "failed" | "aborted" | "refused" => Move::Failed(format!("{s}: {reason}")),
            _ => Move::Progress(s.to_string()),
        },
    }
}

/// A live migration as the VMI and the migration object say it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Migration {
    /// Nothing has started it (no `migrationState`, the object not past Pending).
    Unanswered,
    Running(String),
    Done { target: String },
    Failed(String),
}

pub fn migration(vmi: &Value, mig: &Value) -> Migration {
    let ms = &vmi["status"]["migrationState"];
    let phase = mig["status"]["phase"].as_str().unwrap_or("");
    let reason = ms["failureReason"].as_str().or(mig["status"]["message"].as_str()).unwrap_or("no failureReason");
    if ms["failed"] == true || phase == "Failed" {
        return Migration::Failed(reason.to_string());
    }
    if ms["completed"] == true || phase == "Succeeded" {
        let target = ms["targetNode"].as_str().or(vmi["status"]["nodeName"].as_str()).unwrap_or("").to_string();
        return Migration::Done { target };
    }
    if ms.is_object() || !matches!(phase, "" | "Pending") {
        let at = if phase.is_empty() { "migrationState set".to_string() } else { format!("phase {phase}") };
        return Migration::Running(at);
    }
    Migration::Unanswered
}

/// A migration that failed because the kubelet has no migration settings
/// (stormcos#682): rustkube-node#40 names what is missing.
pub fn settings_missing(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    ["stormstorage", "migration-tls", "migration tls", "migration pair", "not configured", "--migration"].iter().any(|k| r.contains(k))
}

/// The longest stretch with no answer in a run of probes (time from the
/// start, answered?): from the last answer before a miss to the next answer.
/// A run that ends unanswered counts to its last probe.
pub fn longest_gap(samples: &[(Duration, bool)]) -> Duration {
    let mut worst = Duration::ZERO;
    let mut last_ok: Option<Duration> = None;
    let mut missing_since: Option<Duration> = None;
    for &(t, ok) in samples {
        if ok {
            if let Some(m) = missing_since.take() {
                worst = worst.max(t - last_ok.unwrap_or(m));
            }
            last_ok = Some(t);
        } else if missing_since.is_none() {
            missing_since = Some(t);
        }
    }
    if let (Some(m), Some(&(end, _))) = (missing_since, samples.last()) {
        worst = worst.max(end - last_ok.unwrap_or(m));
    }
    worst
}

/// The claim helper's line in a pod log: (state, detail, sha256).
pub fn claim_line(log: &str) -> Option<(String, String, String)> {
    log.lines().filter_map(|l| serde_json::from_str::<Value>(&l.replace('\u{b7}', " ")).ok()).find_map(|v| {
        Some((v["claim"].as_str()?.to_string(), v["detail"].as_str().unwrap_or("").to_string(), v["sha256"].as_str().unwrap_or("").to_string()))
    })
}

fn secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

// ---------------------------------------------------------------- probes

/// TCP probes of `addr` every 200 ms until stopped: (since start, answered).
struct Prober {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Vec<(Duration, bool)>>,
}

impl Prober {
    fn start(addr: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let task = tokio::spawn(async move {
            let t0 = Instant::now();
            let mut v = Vec::new();
            while !s.load(Ordering::Relaxed) {
                let at = t0.elapsed();
                let ok = matches!(tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(&addr)).await, Ok(Ok(_)));
                v.push((at, ok));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            v
        });
        Prober { stop, task }
    }

    async fn finish(self) -> Vec<(Duration, bool)> {
        self.stop.store(true, Ordering::Relaxed);
        self.task.await.unwrap_or_default()
    }
}

// ---------------------------------------------------------------- the run

struct Run {
    kube: Client,
    ns: String,
    run: String,
    id: String,
    image: String,
    a: Args,
    nodes: Vec<String>,
    end: Instant,
}

enum Outcome {
    Pass(String),
    Fail(String),
    Skip(String),
}

fn running_ready(p: &Value) -> bool {
    p["status"]["phase"] == "Running" && p["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

fn restarts(p: &Value) -> u64 {
    p["status"]["containerStatuses"].as_array().into_iter().flatten().filter_map(|c| c["restartCount"].as_u64()).sum()
}

impl Run {
    fn pods(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.ns)
    }

    fn pvcs(&self) -> String {
        format!("/api/v1/namespaces/{}/persistentvolumeclaims", self.ns)
    }

    fn left(&self) -> Duration {
        self.end.saturating_duration_since(Instant::now())
    }

    fn labels(&self) -> Value {
        json!({"storm.io/test-run": self.run, "app.kubernetes.io/managed-by": "stormcos_qa-mobility"})
    }

    async fn create(&self, path: &str, body: &Value) -> Result<Value> {
        let r = self.kube.post(path, body).await?;
        if !r.ok() {
            return Err(anyhow!("creating {} answered {}: {}", body["metadata"]["name"], r.code, r.body));
        }
        Ok(r.body)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let r = self.kube.get(path).await?;
        if !r.ok() {
            return Err(anyhow!("GET {path} answered {}: {}", r.code, r.body));
        }
        Ok(r.body)
    }

    async fn annotate(&self, path: &str, ann: Value) -> Result<()> {
        let r = self.kube.merge_patch(path, &json!({"metadata": {"annotations": ann}})).await?;
        if !r.ok() {
            return Err(anyhow!("annotating {path} answered {}: {}", r.code, r.body));
        }
        Ok(())
    }

    /// Delete and wait until it is gone (bounded).
    async fn gone(&self, path: &str, within: Duration) -> bool {
        let _ = self.kube.delete(path).await;
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if matches!(self.kube.get(path).await, Ok(r) if r.code == 404) {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        false
    }

    fn claim_pod(&self, name: &str, claim: &str, token: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": name, "labels": self.labels()},
               "spec": {"restartPolicy": "Always", "terminationGracePeriodSeconds": 1, "automountServiceAccountToken": false,
                        "containers": [{"name": "c", "image": self.image, "command": ["/test"], "args": ["claim"],
                                        "env": [{"name": "CLAIM_TOKEN", "value": token}],
                                        "readinessProbe": {"tcpSocket": {"port": SERVE_PORT}, "periodSeconds": 1, "failureThreshold": 1},
                                        "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
                        "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": claim}}]}})
    }

    /// Start a claim pod and wait until it is Ready and has said what its
    /// claim holds: (pod, state, sha256, Ready at).
    async fn claim_up(&self, name: &str, claim: &str, token: &str) -> Result<(Value, String, String, Instant)> {
        self.create(&self.pods(), &self.claim_pod(name, claim, token)).await?;
        let path = format!("{}/{name}", self.pods());
        let deadline = Instant::now() + Duration::from_secs(self.a.pod_timeout);
        let mut ready_at = None;
        loop {
            let p = self.get(&path).await?;
            if ready_at.is_none() && running_ready(&p) {
                ready_at = Some(Instant::now());
            }
            if let Some(at) = ready_at {
                let log = self.kube.text(&format!("{path}/log")).await.map(|(_, t)| t).unwrap_or_default();
                if let Some((state, detail, sha)) = claim_line(&log) {
                    if state != "written" && state != "found" {
                        return Err(anyhow!("{name} on {}: claim {state}: {detail}", p["spec"]["nodeName"].as_str().unwrap_or("?")));
                    }
                    return Ok((p, state, sha, at));
                }
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "{name} not Ready with its claim's line in {} s: phase {}, node {}, {}",
                    self.a.pod_timeout,
                    p["status"]["phase"].as_str().unwrap_or("?"),
                    p["spec"]["nodeName"].as_str().unwrap_or("none"),
                    p["status"]["conditions"]
                ));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Wait for a move on `path`: the first state in `want`, a failure, or
    /// nothing at all within `--mechanism-wait`.
    async fn wait_move(&self, path: &str, want: &[Move]) -> Result<Result<Move, Move>> {
        let t = Instant::now();
        let answer_by = t + Duration::from_secs(self.a.mechanism_wait);
        let deadline = t + Duration::from_secs(self.a.move_timeout);
        loop {
            let st = move_state(&self.get(path).await?);
            match &st {
                s if want.contains(s) => return Ok(Ok(st)),
                Move::Failed(_) => return Ok(Err(st)),
                Move::Unanswered if Instant::now() >= answer_by => return Ok(Err(Move::Unanswered)),
                _ if Instant::now() >= deadline => return Ok(Err(st)),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// Tests 1, 4 and 5: a claim in use moves to another node, set, or both.
async fn claim_move(r: &Run, tag: &str, to_node: bool, set: Option<&str>, issue: &str) -> Result<Outcome> {
    let claim = format!("mob-{}-{tag}", r.id);
    let token = format!("{}-{}-{tag}", r.run, r.id);
    let mut pvc = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim, "labels": r.labels()},
        "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "64Mi"}}}});
    if let Some(c) = &r.a.storage_class {
        pvc["spec"]["storageClassName"] = json!(c);
    }
    let pvc_path = format!("{}/{claim}", r.pvcs());
    r.create(&r.pvcs(), &pvc).await?;
    let first = format!("{claim}-a");
    let (p, state, sha_a, _) = r.claim_up(&first, &claim, &token).await?;
    if state != "written" {
        return Ok(Outcome::Fail(format!("a new claim already held data ({state})")));
    }
    let a = p["spec"]["nodeName"].as_str().unwrap_or("").to_string();
    let uid = p["metadata"]["uid"].as_str().unwrap_or("").to_string();
    let restarts_before = restarts(&p);
    let ip = p["status"]["podIP"].as_str().unwrap_or("").to_string();
    let mut ann = json!({});
    let mut asked = Vec::new();
    let b = if to_node {
        let Some(b) = other(&r.nodes, &a) else { return Ok(Outcome::Skip(format!("no Ready node but {a} ({TWO_NODES})"))) };
        ann[MOVE_TO] = json!(b);
        asked.push(format!("node {b}"));
        b
    } else {
        a.clone()
    };
    if let Some(s) = set {
        ann[MOVE_SET] = json!(s);
        asked.push(format!("set {s}"));
    }
    let probe = Prober::start(format!("{ip}:{SERVE_PORT}"));
    let t_req = Instant::now();
    r.annotate(&pvc_path, ann).await?;
    // Node moves cut over once the pod is gone: wait for synced. A set move
    // on the same node runs under the pod: wait for complete.
    let want: &[Move] = if to_node { &[Move::Synced, Move::Complete] } else { &[Move::Complete] };
    let moved = r.wait_move(&pvc_path, want).await?;
    let samples = probe.finish().await;
    let copy = t_req.elapsed();
    match moved {
        Err(Move::Unanswered) => {
            return Ok(Outcome::Skip(format!("{issue}: the claim's move to {} was not taken up in {} s (no {MOVE_STATE})", asked.join(", "), r.a.mechanism_wait)));
        }
        Err(Move::Failed(why)) => return Ok(Outcome::Fail(format!("claim {claim} on {a}, move to {}: {why}", asked.join(", ")))),
        Err(st) => return Ok(Outcome::Fail(format!("claim {claim} on {a}, move to {}: still {st:?} after {} s", asked.join(", "), r.a.move_timeout))),
        Ok(_) => {}
    }
    let first_path = format!("{}/{first}", r.pods());
    if !to_node {
        // Test 4: the pod kept running through it, or the pause is stated.
        let now = r.get(&first_path).await?;
        let kept = now["metadata"]["uid"].as_str() == Some(uid.as_str()) && restarts(&now) == restarts_before && running_ready(&now);
        let pause = longest_gap(&samples);
        // The bytes: a fresh pod reads the claim back.
        r.gone(&first_path, Duration::from_secs(r.a.pod_timeout)).await;
        let (p2, state, sha_b, _) = r.claim_up(&format!("{claim}-b"), &claim, &token).await?;
        let on = p2["spec"]["nodeName"].as_str().unwrap_or("?");
        let base = format!("claim {claim} on {a} moved to {} in {} (copy time); pause {}", asked.join(", "), secs(copy), secs(pause));
        return Ok(if state == "found" && sha_b == sha_a && kept {
            Outcome::Pass(format!("{base}; pod kept running; bytes equal on {on} (sha256 {})", &sha_a[..12.min(sha_a.len())]))
        } else if state == "found" && sha_b == sha_a {
            Outcome::Pass(format!("{base}; the pod did not keep running (uid or restarts changed): pause stated; bytes equal on {on}"))
        } else {
            Outcome::Fail(format!("{base}; claim read back on {on}: {state}, sha256 {sha_b} vs {sha_a} written"))
        });
    }
    // Tests 1 and 5: the eviction's stand-in, then a replacement on B.
    let t_evict = Instant::now();
    if !r.gone(&first_path, Duration::from_secs(r.a.pod_timeout)).await {
        return Ok(Outcome::Fail(format!("{first} on {a} not gone in {} s", r.a.pod_timeout)));
    }
    let up = r.claim_up(&format!("{claim}-b"), &claim, &token).await;
    let state_now = move_state(&r.get(&pvc_path).await?);
    let (p2, state, sha_b, ready) = match up {
        Ok(x) => x,
        Err(e) => return Ok(Outcome::Fail(format!("synced in {}, pod deleted, then: {e:#} (claim {state_now:?})", secs(copy)))),
    };
    let on = p2["spec"]["nodeName"].as_str().unwrap_or("?").to_string();
    let pause = ready.duration_since(t_evict);
    let base = format!("claim {claim}: {a} → {}; synced in {}; eviction → Ready on {on} {}", asked.join(", "), secs(copy), secs(pause));
    Ok(if on == b && state == "found" && sha_b == sha_a {
        Outcome::Pass(format!("{base}; sha256 of the file set equal ({}); move {state_now:?}", &sha_a[..12.min(sha_a.len())]))
    } else {
        Outcome::Fail(format!("{base}; wanted on {b}: claim {state}, sha256 {sha_b} vs {sha_a} written"))
    })
}

// ---------------------------------------------------------------- VMs

struct Vm<'a> {
    r: &'a Run,
    name: String,
    key: &'a ssh::Key,
    secret: String,
}

impl Vm<'_> {
    fn path(&self) -> String {
        format!("{}/{}", kube::vms(&self.r.ns), self.name)
    }

    fn vmi_path(&self) -> String {
        format!("{}/{}", kube::vmis(&self.r.ns), self.name)
    }

    /// qemu, a golden-clone root plus an emptyDisk, on the node bridge: what
    /// the kubelet will move (it refuses claim disks, pod-network VMIs and
    /// cloud-hypervisor, stormcos docs/MOBILITY.md).
    fn manifest(&self) -> Value {
        let r = self.r;
        json!({"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachine",
            "metadata": {"name": self.name, "namespace": r.ns, "labels": r.labels()},
            "spec": {"running": true, "template": {
                "metadata": {"labels": r.labels(), "annotations": {"storm.io/bridge": r.a.bridge}},
                "spec": {
                    "domain": {"cpu": {"cores": 1},
                               "memory": {"guest": format!("{}Mi", r.a.vm_memory_mib)},
                               "resources": {"requests": {"memory": format!("{}Mi", r.a.vm_memory_mib)}},
                               "devices": {"disks": [{"name": "root", "disk": {"bus": "virtio"}},
                                                     {"name": "scratch", "disk": {"bus": "virtio"}},
                                                     {"name": "seed", "disk": {"bus": "virtio"}}],
                                           "interfaces": [{"name": "default", "bridge": {}}]}},
                    "networks": [{"name": "default", "pod": {}}],
                    "accessCredentials": [{"sshPublicKey": {"source": {"secret": {"secretName": self.secret}}, "propagationMethod": {"noCloud": {}}}}],
                    "volumes": [{"name": "root", "dataVolume": {"name": r.a.golden}},
                                {"name": "scratch", "emptyDisk": {"capacity": "1Gi"}},
                                {"name": "seed", "cloudInitNoCloud": {"userData": "#cloud-config\n"}}]}}}})
    }

    /// Wait for the VMI Running with an address and ssh: (uid, ip, node).
    async fn up(&self, not_uid: Option<&str>) -> Result<(String, String, String)> {
        let deadline = Instant::now() + Duration::from_secs(self.r.a.vm_timeout);
        let mut last = String::from("no VMI yet");
        while Instant::now() < deadline {
            if let Ok(rp) = self.r.kube.get(&self.vmi_path()).await
                && rp.ok()
            {
                let v = &rp.body;
                let uid = v["metadata"]["uid"].as_str().unwrap_or("").to_string();
                let ip = v["status"]["interfaces"].as_array().into_iter().flatten().filter_map(|i| i["ipAddress"].as_str()).find(|s| !s.is_empty()).unwrap_or("").to_string();
                let node = v["status"]["nodeName"].as_str().unwrap_or("").to_string();
                if not_uid == Some(uid.as_str()) {
                    last = "the old VMI is still there".into();
                } else if v["status"]["phase"] == "Running" && !ip.is_empty() {
                    if self.ssh(&ip, "true").await.is_ok() {
                        return Ok((uid, ip, node));
                    }
                    last = format!("Running at {ip} on {node}, ssh not yet");
                } else {
                    last = format!("phase {:?}, reason {:?}, message {:?}", v["status"]["phase"], v["status"]["reason"], v["status"]["message"]);
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        if last.contains("waiting for golden") {
            return Err(retry::Infra { what: format!("VM {}", self.name), attempts: 1, elapsed: Duration::from_secs(self.r.a.vm_timeout), last, unsure: false }.into());
        }
        Err(anyhow!("VM {} not up with ssh in {} s: {last}", self.name, self.r.a.vm_timeout))
    }

    async fn ssh(&self, ip: &str, cmd: &str) -> Result<String> {
        ssh::run(&format!("{ip}:22"), &self.r.a.ssh_user, self.key.private.clone(), cmd, Duration::from_secs(30)).await
    }

    async fn running(&self, on: bool) -> Result<()> {
        let rp = self.r.kube.merge_patch(&self.path(), &json!({"spec": {"running": on}})).await?;
        if !rp.ok() {
            return Err(anyhow!("setting {} running={on} answered {}: {}", self.name, rp.code, rp.body));
        }
        Ok(())
    }
}

/// The guest's checksum loop: write 1 MiB, read it back past the page cache,
/// compare; count rounds, record any mismatch. Detached from the ssh session.
const LOOP: &str = "setsid nohup sh -c 'n=0; while :; do \
    dd if=/dev/urandom of=/var/tmp/mob.f bs=1M count=1 oflag=direct 2>/dev/null; \
    a=$(sha256sum < /var/tmp/mob.f); b=$(dd if=/var/tmp/mob.f bs=1M iflag=direct 2>/dev/null | sha256sum); \
    [ \"$a\" = \"$b\" ] || echo \"round $n\" >> /var/tmp/mob.fail; n=$((n+1)); echo $n > /var/tmp/mob.n; sleep 0.2; done' \
    </dev/null >/dev/null 2>&1 & sleep 1; cat /var/tmp/mob.n";
const STATE: &str = "cat /proc/sys/kernel/random/boot_id; cut -d' ' -f1 /proc/uptime; cat /var/tmp/mob.n 2>/dev/null || echo 0; cat /var/tmp/mob.fail 2>/dev/null | wc -l";

/// (boot id, uptime, rounds, failures) from [`STATE`]'s output.
pub fn guest_state(s: &str) -> Option<(String, f64, u64, u64)> {
    let mut l = s.lines().map(str::trim);
    Some((l.next()?.to_string(), l.next()?.parse().ok()?, l.next()?.parse().ok()?, l.next()?.parse().ok()?))
}

/// Test 2.
async fn vm_live(vm: &Vm<'_>) -> Result<Outcome> {
    let r = vm.r;
    let migs = format!("/apis/kubevirt.io/v1/namespaces/{}/virtualmachineinstancemigrations", r.ns);
    match r.kube.get(&migs).await? {
        x if x.code == 404 => return Ok(Outcome::Skip("no VirtualMachineInstanceMigration API on the apiserver (rustkube#184)".into())),
        x if !x.ok() => return Err(retry::Infra { what: format!("GET {migs}"), attempts: 1, elapsed: Duration::ZERO, last: format!("{}: {}", x.code, x.body), unsure: false }.into()),
        _ => {}
    }
    r.create(&kube::vms(&r.ns), &vm.manifest()).await?;
    let (uid, ip, a) = vm.up(None).await?;
    let Some(b) = other(&r.nodes, &a) else { return Ok(Outcome::Skip(format!("no Ready node but {a} ({TWO_NODES})"))) };
    vm.ssh(&ip, LOOP).await?;
    let before = guest_state(&vm.ssh(&ip, STATE).await?).ok_or_else(|| anyhow!("guest state unreadable"))?;
    let probe = Prober::start(format!("{ip}:22"));
    let name = format!("{}-mig", vm.name);
    let t0 = Instant::now();
    r.create(&migs, &json!({"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstanceMigration",
        "metadata": {"name": name, "labels": r.labels()}, "spec": {"vmiName": vm.name}})).await?;
    let answer_by = t0 + Duration::from_secs(r.a.mechanism_wait);
    let deadline = t0 + Duration::from_secs(r.a.move_timeout);
    let end = loop {
        let vmi = r.get(&vm.vmi_path()).await?;
        let m = r.kube.get(&format!("{migs}/{name}")).await.map(|x| x.body).unwrap_or(Value::Null);
        let st = migration(&vmi, &m);
        match &st {
            Migration::Done { .. } | Migration::Failed(_) => break st,
            Migration::Unanswered if Instant::now() >= answer_by => break st,
            _ if Instant::now() >= deadline => break st,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    let total = t0.elapsed();
    // Some answers after the end, so a gap at the very end is closed.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let samples = probe.finish().await;
    let target = match end {
        Migration::Unanswered => return Ok(Outcome::Skip(format!("{VM_LIVE}: migration {name} not started in {} s (no migrationState)", r.a.mechanism_wait))),
        Migration::Failed(why) if settings_missing(&why) => return Ok(Outcome::Skip(format!("{VM_LIVE}: the kubelet's migration settings: {why}"))),
        Migration::Failed(why) => return Ok(Outcome::Fail(format!("migration of {} from {a}: {why}", vm.name))),
        Migration::Running(at) => return Ok(Outcome::Fail(format!("migration of {} from {a} not done in {} s ({at})", vm.name, r.a.move_timeout))),
        Migration::Done { target } => target,
    };
    let vmi = r.get(&vm.vmi_path()).await?;
    let on = vmi["status"]["nodeName"].as_str().unwrap_or(&target).to_string();
    let after = guest_state(&vm.ssh(&ip, STATE).await?).ok_or_else(|| anyhow!("guest state unreadable after the migration"))?;
    let downtime = longest_gap(&samples);
    let base = format!("{} {a} → {on} (wanted {b}): total {}, downtime {} (guest :22)", vm.name, secs(total), secs(downtime));
    let mut bad = Vec::new();
    if on == a {
        bad.push(format!("still on {a}"));
    }
    if vmi["metadata"]["uid"].as_str() != Some(uid.as_str()) {
        bad.push("a new VMI (not a live migration)".into());
    }
    if after.0 != before.0 || after.1 < before.1 {
        bad.push(format!("guest rebooted: boot id {} → {}, uptime {} → {}", before.0, after.0, before.1, after.1));
    }
    if after.3 > 0 {
        bad.push(format!("the checksum loop failed {} time(s)", after.3));
    }
    if after.2 <= before.2 {
        bad.push(format!("the checksum loop stopped (rounds {} → {})", before.2, after.2));
    }
    Ok(if bad.is_empty() {
        Outcome::Pass(format!("{base}; uptime unbroken ({:.0} → {:.0} s), checksum loop {} rounds, 0 failures", before.1, after.1, after.2))
    } else {
        Outcome::Fail(format!("{base}; {}", bad.join("; ")))
    })
}

/// Test 3.
async fn vm_cold(vm: &Vm<'_>) -> Result<Outcome> {
    let r = vm.r;
    r.create(&kube::vms(&r.ns), &vm.manifest()).await?;
    let (uid, ip, a) = vm.up(None).await?;
    let Some(b) = other(&r.nodes, &a) else { return Ok(Outcome::Skip(format!("no Ready node but {a} ({TWO_NODES})"))) };
    let sha = vm.ssh(&ip, "head -c 1048576 /dev/urandom > /var/tmp/mob.cold && sync && sha256sum < /var/tmp/mob.cold").await?.trim().to_string();
    vm.running(false).await?;
    if !wait_404(r, &vm.vmi_path(), Duration::from_secs(r.a.vm_timeout)).await {
        return Ok(Outcome::Fail(format!("{} on {a}: VMI not gone in {} s after stop", vm.name, r.a.vm_timeout)));
    }
    let t_stop = Instant::now();
    r.annotate(&vm.path(), json!({MOVE_TO: b})).await?;
    match r.wait_move(&vm.path(), &[Move::Complete]).await? {
        Err(Move::Unanswered) => return Ok(Outcome::Skip(format!("{VM_COLD}: the stopped VM's move to {b} was not taken up in {} s (no {MOVE_STATE})", r.a.mechanism_wait))),
        Err(Move::Failed(why)) => return Ok(Outcome::Fail(format!("{} on {a}, move to {b}: {why}", vm.name))),
        Err(st) => return Ok(Outcome::Fail(format!("{} on {a}, move to {b}: still {st:?} after {} s", vm.name, r.a.move_timeout))),
        Ok(_) => {}
    }
    let moved = t_stop.elapsed();
    vm.running(true).await?;
    let (_, ip2, on) = vm.up(Some(&uid)).await?;
    let up = t_stop.elapsed();
    let got = vm.ssh(&ip2, "sha256sum < /var/tmp/mob.cold").await.unwrap_or_else(|e| format!("unreadable: {e:#}")).trim().to_string();
    let base = format!("{} {a} → {on} (wanted {b}): disks moved in {}, stop → guest up {}", vm.name, secs(moved), secs(up));
    Ok(if on == b && got == sha {
        Outcome::Pass(format!("{base}; booted from them, file sha256 equal"))
    } else {
        Outcome::Fail(format!("{base}; file sha256 {got} vs {sha} written"))
    })
}

async fn wait_404(r: &Run, path: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if matches!(r.kube.get(path).await, Ok(x) if x.code == 404) {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

fn emit(out: &Out, test: &str, t: Instant, o: Result<Outcome>) {
    match o {
        Ok(Outcome::Pass(d)) => out.emit(Line::new(test, Status::Pass, t.elapsed(), d)),
        Ok(Outcome::Fail(d)) => out.emit(Line::new(test, Status::Fail, t.elapsed(), d)),
        Ok(Outcome::Skip(d)) => out.emit(Line::new(test, Status::Skip, t.elapsed(), d)),
        Err(e) => out.failure(test, t.elapsed(), &e, format!("{e:#}")),
    }
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "mobility");
    let t = Instant::now();
    let kube = match Client::new(&a.api, a.token_file.as_deref(), a.insecure).await {
        Ok(k) => k,
        Err(e) => {
            out.could_not_run("api", t.elapsed(), format!("{e:#}"));
            out.summary();
            return 2;
        }
    };
    let ns = a
        .namespace
        .clone()
        .unwrap_or_else(|| std::fs::read_to_string(format!("{SA}/namespace")).unwrap_or_else(|_| "default".into()).trim().to_string());
    let nodes = match kube.get("/api/v1/nodes").await {
        Ok(r) if r.ok() => movable_nodes(&kube::items(&r.body)),
        Ok(r) => {
            out.could_not_run("mobility/nodes", t.elapsed(), format!("/api/v1/nodes answered {} (requires.toml cluster_read)", r.code));
            out.summary();
            return out.exit_code();
        }
        Err(e) => {
            out.failure("mobility/nodes", t.elapsed(), &e, format!("/api/v1/nodes: {e:#}"));
            out.summary();
            return out.exit_code();
        }
    };
    if nodes.len() < 2 {
        for test in TESTS {
            out.emit(Line::new(test, Status::Skip, t.elapsed(), format!("{} Ready schedulable node(s) [{}]: a move needs two (a two-node cluster, {TWO_NODES})", nodes.len(), nodes.join(" "))));
        }
        out.summary();
        return out.exit_code();
    }
    let image = match a.image.clone() {
        Some(i) => Some(i),
        None => kube::own_image(&kube, &ns, &a.run_id).await,
    };
    let Some(image) = image else {
        for test in TESTS {
            out.could_not_run(test, Duration::ZERO, "this test's own image is unknown (not a runner pod and no --image)");
        }
        out.summary();
        return out.exit_code();
    };
    let id: String = a.run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let end = t + Duration::from_secs(a.timeout.saturating_sub(60));
    let run_label = a.run_id.clone();
    let r = Run { kube, ns, run: run_label, id, image, nodes, end, a };
    eprintln!("mobility: nodes {}, image {}", r.nodes.join(" "), r.image);

    // Claims first (cheap), then VMs. A test whose worst case no longer fits
    // the window is a skip, so cleanup always runs.
    let claim_worst = Duration::from_secs(3 * r.a.pod_timeout + r.a.move_timeout);
    let short = |worst: Duration| format!("{} s of the window left, the test may take {} s", r.left().as_secs(), worst.as_secs());
    let tt = Instant::now();
    let o = if r.left() < claim_worst { Ok(Outcome::Skip(short(claim_worst))) } else { claim_move(&r, "n", true, None, CLAIM_NODE).await };
    emit(&out, "move/claim-node", tt, o);
    out.emit(Line::new("move/drain", Status::Skip, Duration::ZERO, format!("a Job gets no node writes: {DRAIN}")));
    for (test, tag, to_node, issue) in [("move/claim-set", "s", false, CLAIM_SET), ("move/claim-node-set", "ns", true, CLAIM_NODE_SET)] {
        let tt = Instant::now();
        let o = match r.a.raid_set.as_deref() {
            None => Ok(Outcome::Skip(NO_SET.into())),
            Some(_) if r.left() < claim_worst => Ok(Outcome::Skip(short(claim_worst))),
            Some(set) => claim_move(&r, tag, to_node, Some(set), issue).await,
        };
        emit(&out, test, tt, o);
    }
    cleanup_claims(&r).await;

    let vm_worst = Duration::from_secs(2 * r.a.vm_timeout + r.a.move_timeout + 60);
    if r.a.no_vms {
        for test in ["move/vm-live", "move/vm-cold"] {
            out.emit(Line::new(test, Status::Skip, Duration::ZERO, "--no-vms"));
        }
    } else {
        match vm_setup(&r).await {
            Err(e) => {
                for test in ["move/vm-live", "move/vm-cold"] {
                    out.failure(test, Duration::ZERO, &e, format!("{e:#}"));
                }
            }
            Ok((key, secret)) => {
                for (test, tag) in [("move/vm-live", "live"), ("move/vm-cold", "cold")] {
                    let tt = Instant::now();
                    let vm = Vm { r: &r, name: format!("mob-{}-{tag}", r.id), key: &key, secret: secret.clone() };
                    let o = if r.left() < vm_worst {
                        Ok(Outcome::Skip(short(vm_worst)))
                    } else if tag == "live" {
                        vm_live(&vm).await
                    } else {
                        vm_cold(&vm).await
                    };
                    emit(&out, test, tt, o);
                    let _ = r.kube.delete(&format!("/apis/kubevirt.io/v1/namespaces/{}/virtualmachineinstancemigrations/{}-mig", r.ns, vm.name)).await;
                    r.gone(&vm.path(), Duration::from_secs(120)).await;
                }
                let _ = r.kube.delete(&format!("/api/v1/namespaces/{}/secrets/{secret}", r.ns)).await;
            }
        }
    }
    out.summary();
    out.exit_code()
}

/// The run's ssh key and its Secret for the VMs' accessCredentials.
async fn vm_setup(r: &Run) -> Result<(ssh::Key, String)> {
    let key = ssh::Key::generate()?;
    let secret = format!("mob-{}-ssh", r.id);
    let path = format!("/api/v1/namespaces/{}/secrets", r.ns);
    let _ = r.kube.delete(&format!("{path}/{secret}")).await;
    r.create(&path, &json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": secret, "labels": r.labels()}, "stringData": {"key": key.public_openssh}}))
        .await?;
    Ok((key, secret))
}

async fn cleanup_claims(r: &Run) {
    for tag in ["n", "s", "ns"] {
        let claim = format!("mob-{}-{tag}", r.id);
        for p in ["a", "b"] {
            let _ = r.kube.delete(&format!("{}/{claim}-{p}", r.pods())).await;
        }
        let _ = r.kube.delete(&format!("{}/{claim}", r.pvcs())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, ready: bool, unschedulable: bool) -> Value {
        json!({"metadata": {"name": name}, "spec": {"unschedulable": unschedulable},
               "status": {"conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }

    #[test]
    fn only_ready_schedulable_nodes_move() {
        let ns = movable_nodes(&[node("b", true, false), node("a", true, false), node("c", false, false), node("d", true, true)]);
        assert_eq!(ns, ["a", "b"]);
        assert_eq!(other(&ns, "a").as_deref(), Some("b"));
        assert_eq!(other(&["a".to_string()], "a"), None);
    }

    #[test]
    fn move_states_from_the_annotations() {
        let o = |a: Value| json!({"metadata": {"annotations": a}});
        assert_eq!(move_state(&json!({"metadata": {}})), Move::Unanswered);
        assert_eq!(move_state(&o(json!({MOVE_TO: "b"}))), Move::Unanswered);
        assert_eq!(move_state(&o(json!({MOVE_STATE: "mirroring"}))), Move::Progress("mirroring".into()));
        assert_eq!(move_state(&o(json!({MOVE_STATE: "Synced"}))), Move::Synced);
        assert_eq!(move_state(&o(json!({MOVE_STATE: "complete"}))), Move::Complete);
        assert_eq!(move_state(&o(json!({MOVE_STATE: "done"}))), Move::Complete);
        assert_eq!(move_state(&o(json!({MOVE_STATE: "refused", MOVE_REASON: "ReadWriteMany"}))), Move::Failed("refused: ReadWriteMany".into()));
        assert_eq!(move_state(&o(json!({MOVE_STATE: "aborted"}))), Move::Failed("aborted: no reason given".into()));
    }

    #[test]
    fn migration_states() {
        let vmi = |ms: Value| json!({"status": {"nodeName": "a", "migrationState": ms}});
        assert_eq!(migration(&json!({"status": {}}), &json!({"status": {"phase": "Pending"}})), Migration::Unanswered);
        assert_eq!(migration(&json!({"status": {}}), &Value::Null), Migration::Unanswered);
        assert!(matches!(migration(&vmi(json!({"startTimestamp": "x"})), &Value::Null), Migration::Running(_)));
        assert!(matches!(migration(&json!({"status": {}}), &json!({"status": {"phase": "Running"}})), Migration::Running(_)));
        assert_eq!(migration(&vmi(json!({"completed": true, "targetNode": "b"})), &Value::Null), Migration::Done { target: "b".into() });
        let f = migration(&vmi(json!({"failed": true, "failureReason": "no --stormstorage set on the kubelet"})), &Value::Null);
        assert_eq!(f, Migration::Failed("no --stormstorage set on the kubelet".into()));
        assert!(settings_missing("no --stormstorage set on the kubelet"));
        assert!(settings_missing("--migration-tls-cert is not set"));
        assert!(!settings_missing("target refused: out of memory"));
        assert_eq!(migration(&json!({"status": {}}), &json!({"status": {"phase": "Failed", "message": "m"}})), Migration::Failed("m".into()));
    }

    #[test]
    fn the_longest_gap_runs_from_the_last_answer_to_the_next() {
        let ms = |m: u64| Duration::from_millis(m);
        let s = [(ms(0), true), (ms(200), true), (ms(400), false), (ms(600), false), (ms(800), true), (ms(1000), false), (ms(1200), true)];
        assert_eq!(longest_gap(&s), ms(600));
        assert_eq!(longest_gap(&[(ms(0), true), (ms(200), true)]), Duration::ZERO);
        assert_eq!(longest_gap(&[(ms(0), true), (ms(200), false), (ms(900), false)]), ms(900));
        assert_eq!(longest_gap(&[(ms(100), false), (ms(500), true)]), ms(400));
        assert_eq!(longest_gap(&[]), Duration::ZERO);
    }

    #[test]
    fn claim_lines_carry_the_file_set_digest() {
        let l = crate::report::unspaced(&json!({"claim": "found", "detail": "token and blob intact", "sha256": "ab12"}).to_string());
        assert_eq!(claim_line(&format!("noise\n{l}\n")), Some(("found".into(), "token and blob intact".into(), "ab12".into())));
        assert_eq!(claim_line("{\"claim\":\"written\"}").map(|c| c.2), Some(String::new()));
        assert_eq!(claim_line("nothing"), None);
    }

    #[test]
    fn guest_state_parses() {
        assert_eq!(guest_state("abc\n12.5\n40\n0\n"), Some(("abc".into(), 12.5, 40, 0)));
        assert_eq!(guest_state("abc\n12.5\n"), None);
    }

    #[test]
    fn every_line_names_its_issue() {
        assert_eq!(TESTS.len(), 6);
        for i in [CLAIM_NODE, CLAIM_SET, CLAIM_NODE_SET, VM_COLD, VM_LIVE, DRAIN, NO_SET, TWO_NODES] {
            assert!(i.contains('#'), "{i}");
        }
    }
}
