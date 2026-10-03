//! `/test turbomode` — the explicit load test (#26; owner's choice A on
//! #33): two profiles, run one after the other, in the run's own namespace:
//!
//! - **sleep**: N Pods, each running `/test sleep <secs>`; every Pod must
//!   reach Succeeded;
//! - **sqlite**: M Pods, each with its own fresh PVC, running `/test
//!   sqlite`: 1,000 SQLite records written, read back and checked, a
//!   sleep, checked again (`sqlite.rs`). Every Pod's log must prove it. The
//!   storage audit (`turbo_audit.rs`) records the node `before`, proves the
//!   M volumes `allocated`, and proves nothing of them is left `after`
//!   cleanup — API objects disappearing is not proof of reclamation.
//!
//! Two sizes (#43; owner: by day a golden's test fits 15 min, anything over
//! 30 min runs only at night on a pve VM): `/test turbomode` is the day run
//! (100 sleeping Pods, 25 pairs, 60 s, budget 900 s), `/test
//! turbomode-night` the full scale (1,000 Pods, 100 pairs, 120 s, hour-long
//! finish bounds, budget 14400 s) — the same driver, `NIGHT` flags first.
//!
//! Each profile has bounded **attempts** (`--attempts`, 1..5). Every attempt
//! is a fresh run with its own label, and writes its own evidence under
//! `<results>/turbomode/<profile>/attempt-N/` (report.json, SQLite logs,
//! storage audits). A failed attempt is retried only when every failure in
//! it is *transient* (a create error incl. a lost acknowledgement, partial
//! startup, a sleeping Pod that failed) **and** its cleanup, including the
//! `after` audit, verified: a later pass never hides a leak or corruption.
//! Integrity, storage, cleanup and unexpected failures are final. Counts
//! never change between attempts; a pass after retries says so.
//!
//! Cleanup deletes exactly this attempt's Pods and claims (found by label,
//! so a create whose ack was lost is cleaned too) with UID preconditions,
//! then waits for them, their PVs and VolumeAttachments to be gone. It never
//! removes finalizers or deletes PVs or backing volumes itself: that would
//! conceal broken reclamation.
//!
//! Latency comes from a Pod watch (relisted after an error, which marks the
//! attempt's latency invalid). Output: a JSON line per profile, the
//! per-profile `summary.json`. Exit 0 passed, 1 failed, 2 could not run.
//! A port of `tools/turbomode/run.py`, which stays as the reference.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, Semaphore};

use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};
use crate::turbo_audit::{self, Host, Phase, Request};
use crate::{long, sqlite};

/// The attempt label: `qa.storm.io/turbomode-run=<attempt token>`.
pub const LABEL: &str = "qa.storm.io/turbomode-run";
pub const MANAGED_BY: &str = "stormcos_qa-turbomode";
pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Sleep,
    Sqlite,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Profile::Sleep => "sleep",
            Profile::Sqlite => "sqlite",
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "test turbomode", about = "Load test: 100 sleeping Pods, then 25 Pods with a SQLite PVC each (#26); turbomode-night: 1,000 and 100 (#43)")]
pub struct Args {
    /// Apiserver URL. Empty: in-cluster (service account).
    #[arg(long, env = "STORM_API", default_value = "")]
    api: String,
    /// Bearer token file (default: the service account's).
    #[arg(long)]
    token_file: Option<String>,
    /// Skip TLS verification of the apiserver (outside a cluster).
    #[arg(long)]
    insecure: bool,
    /// The run's own namespace; every Pod and claim is created in it.
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    /// Labels everything as storm.io/test-run=<id>.
    #[arg(long, env = "STORM_RUN_ID", default_value = "manual")]
    run_id: String,
    /// The node under test: its address (stormblock's default host).
    #[arg(long, env = "STORM_NODE", default_value = "127.0.0.1")]
    node: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: PathBuf,
    /// The test's own executable, on the image's volume. Its content is read
    /// from the device at the start and after every attempt; a change is an
    /// integrity failure (stormblock#267). Absent: not checked.
    #[arg(long, default_value = "/test")]
    image_file: PathBuf,
    /// Seconds the whole run may take (the runner's budget_secs): an attempt
    /// starts only if its worst case (finish + cleanup timeouts + 300 s) fits.
    #[arg(long, env = "STORM_TIMEOUT", default_value_t = 900)]
    timeout: u64,
    /// The profiles, in order.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "sleep,sqlite")]
    profiles: Vec<Profile>,
    /// Pods of the sleeping profile.
    #[arg(long, default_value_t = 100)]
    sleep_pods: usize,
    /// Pods (each with its claim) of the SQLite profile.
    #[arg(long, default_value_t = 25)]
    sqlite_pods: usize,
    /// Each Pod's sleep (sqlite: between its two checks).
    #[arg(long, default_value_t = 60)]
    sleep_seconds: u64,
    /// The claims' StorageClass (default: the cluster's default class). It
    /// must reclaim with Delete; it is never changed.
    #[arg(long)]
    storage_class: Option<String>,
    #[arg(long, default_value = "64Mi")]
    claim_size: String,
    /// This test's image, run by the Pods as `/test sleep|sqlite` (default:
    /// the Job pod's own).
    #[arg(long)]
    image: Option<String>,
    /// Bounded attempts per profile (1..5); a retry needs a verified cleanup.
    #[arg(long, default_value_t = 2)]
    attempts: u32,
    /// Seconds between a verified-clean failed attempt and the next.
    #[arg(long, default_value_t = 30)]
    retry_delay: u64,
    /// Per attempt: creates issued → every Pod Succeeded or Failed.
    #[arg(long, default_value_t = 240)]
    finish_timeout: u64,
    /// Per attempt: deletes issued → nothing left (API and the after audit).
    #[arg(long, default_value_t = 120)]
    cleanup_timeout: u64,
    /// Creates in flight at once.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    /// The node this Job runs on (downward API spec.nodeName). The storage
    /// audit sees only it, so the cluster must have exactly this node.
    #[arg(long, env = "TURBOMODE_NODE")]
    node_name: Option<String>,
    /// stormblock's API (default http://<node>:9090).
    #[arg(long, env = "TURBOMODE_STORMBLOCK")]
    stormblock_url: Option<String>,
    /// Where the runner mounts the host's paths read-only (stormcentral#74:
    /// `/proc` at `/host/proc`); the paths below default under it.
    #[arg(long, env = "STORM_HOST_ROOT", default_value = "/")]
    host_root: PathBuf,
    /// Host /proc (default <host root>/proc; hostPID).
    #[arg(long, env = "TURBOMODE_PROC")]
    proc_root: Option<PathBuf>,
    /// Host init's mountinfo (default <proc>/1/mountinfo).
    #[arg(long, env = "TURBOMODE_HOST_MOUNTINFO")]
    host_mountinfo: Option<PathBuf>,
    /// Host cgroup tree (default <host root>/sys/fs/cgroup).
    #[arg(long, env = "TURBOMODE_CGROUP")]
    cgroup_root: Option<PathBuf>,
    /// stormblock's engine token (only this file of /run is mounted;
    /// default <host root>/run/stormblock/engine/api_token).
    #[arg(long, env = "TURBOMODE_TOKEN")]
    stormblock_token: Option<PathBuf>,
    /// The kubelet's pods dir, if mounted (it is not in the owner's list:
    /// per-Pod directories are then recorded unmeasured).
    #[arg(long, env = "TURBOMODE_KUBELET_PODS")]
    kubelet_pods: Option<PathBuf>,
}

// ---------------------------------------------------------------- failures

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Transient,
    Integrity,
    Storage,
    Cleanup,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub kind: Kind,
    pub error: String,
}

impl Failure {
    fn new(kind: Kind, error: impl Into<String>) -> Self {
        Failure { kind, error: error.into() }
    }
}

fn error(e: anyhow::Error) -> Failure {
    Failure::new(Kind::Error, format!("{e:#}"))
}

/// A failed attempt may be retried only if it left nothing behind and every
/// failure in it is transient: a later success must not conceal a leak or
/// corruption.
pub fn retryable(passed: bool, cleanup_verified: bool, failures: &[Failure]) -> bool {
    !passed && cleanup_verified && !failures.is_empty() && failures.iter().all(|f| f.kind == Kind::Transient)
}

/// Seconds an attempt needs beyond its finish and cleanup bounds: creates,
/// the three audits and the report (seconds on pvetest1; the Job deadline
/// adds the runner's 180 s start grace on top).
pub const ATTEMPT_RESERVE: u64 = 120;

/// `/test turbomode-night` (#43): the full scale, run in the night window
/// with `[turbomode-night] budget_secs = 14400`. Put before the caller's
/// flags, so a flag given wins (clap keeps the last).
pub const NIGHT: &[&str] = &[
    "--sleep-pods", "1000", "--sqlite-pods", "100", "--sleep-seconds", "120",
    "--attempts", "3", "--finish-timeout", "3600", "--cleanup-timeout", "600",
];

/// argv for turbomode's parser: `night` puts `NIGHT` after the program name.
pub fn argv(mut argv: Vec<String>, night: bool) -> Vec<String> {
    if night {
        let at = argv.len().min(1);
        argv.splice(at..at, NIGHT.iter().map(|s| s.to_string()));
    }
    argv
}

/// An attempt starts only if its worst case fits in what is left of the run
/// window: the runner's Job deadline is the window (plus a start grace), so an
/// attempt cut off there would leave neither its cleanup nor its result.
pub fn attempt_fits(elapsed: Duration, window: u64, finish_timeout: u64, cleanup_timeout: u64) -> bool {
    elapsed + Duration::from_secs(finish_timeout + cleanup_timeout + ATTEMPT_RESERVE) <= Duration::from_secs(window)
}

/// p50/p95/p99/max (nearest rank) of seconds.
pub fn percentiles(mut v: Vec<f64>) -> Value {
    if v.is_empty() {
        return json!({"samples": 0});
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() as f64 * p / 100.0).ceil() as usize).max(1) - 1];
    json!({"samples": v.len(), "p50": at(50.0), "p95": at(95.0), "p99": at(99.0), "max": v[v.len() - 1]})
}

// ---------------------------------------------------------------- api

/// Percent-encode a query value.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// A paginated LIST; its items and resourceVersion. A list that changes
/// resourceVersion or repeats a continue token midway is an error.
async fn list(kube: &Client, path: &str, selector: Option<&str>) -> Result<(Vec<Value>, String)> {
    let (mut items, mut cont, mut rv, mut seen) = (Vec::new(), String::new(), None::<String>, BTreeSet::new());
    loop {
        let mut q = format!("{path}?limit=500");
        if let Some(s) = selector {
            q += &format!("&labelSelector={}", enc(s));
        }
        if !cont.is_empty() {
            q += &format!("&continue={}", enc(&cont));
        }
        let r = kube.get(&q).await?;
        ensure!(r.ok(), "LIST {path} answered {}: {}", r.code, r.body);
        let v = r.body["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
        if rv.as_ref().is_some_and(|x| *x != v) {
            bail!("paginated LIST {path} changed resourceVersion");
        }
        rv = Some(v);
        items.extend(kube::items(&r.body));
        cont = r.body["metadata"]["continue"].as_str().unwrap_or_default().to_string();
        if cont.is_empty() {
            return Ok((items, rv.unwrap_or_default()));
        }
        if !seen.insert(cont.clone()) {
            bail!("paginated LIST {path} repeated a continue token");
        }
    }
}

fn uid(v: &Value) -> String {
    v["metadata"]["uid"].as_str().unwrap_or_default().to_string()
}

fn name(v: &Value) -> String {
    v["metadata"]["name"].as_str().unwrap_or_default().to_string()
}

fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A file's length and sha256, read from its device: its unmapped pages are
/// dropped from the page cache first, so a volume that returns different
/// bytes shows here (stormblock#267). Mapped pages (a running executable's
/// hot code) stay cached; the rest of the file is what is compared.
pub fn image_digest(path: &Path) -> Result<(u64, String)> {
    use sha2::Digest;
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    // SAFETY: posix_fadvise on a descriptor we own; advice only.
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
    let b = std::fs::read(path).with_context(|| path.display().to_string())?;
    Ok((b.len() as u64, sha2::Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect()))
}

// ---------------------------------------------------------------- observer

#[derive(Default)]
struct ObsState {
    /// uid → scheduled/running/finished/deleted, seconds since the attempt began.
    records: BTreeMap<String, BTreeMap<&'static str, f64>>,
    objects: BTreeMap<String, Value>,
    errors: Vec<String>,
    peak: usize,
}

struct Observer {
    s: Mutex<ObsState>,
    changed: Notify,
    t0: Instant,
}

impl Observer {
    fn observe(&self, pod: &Value, deleted: bool) {
        let at = self.t0.elapsed().as_secs_f64();
        let id = uid(pod);
        let mut s = self.s.lock().unwrap();
        let rec = s.records.entry(id.clone()).or_default();
        if pod["spec"]["nodeName"].as_str().is_some_and(|n| !n.is_empty()) {
            rec.entry("scheduled").or_insert(at);
        }
        match pod["status"]["phase"].as_str() {
            Some("Running") => {
                rec.entry("running").or_insert(at);
            }
            Some("Succeeded") | Some("Failed") => {
                rec.entry("finished").or_insert(at);
            }
            _ => {}
        }
        if deleted {
            rec.insert("deleted", at);
        }
        s.objects.insert(id, pod.clone());
        let running = s
            .objects
            .iter()
            .filter(|(u, p)| p["status"]["phase"] == "Running" && !s.records.get(*u).is_some_and(|r| r.contains_key("deleted")))
            .count();
        s.peak = s.peak.max(running);
        drop(s);
        self.changed.notify_one();
    }

    /// Watch the namespace's Pods with the attempt's label until aborted. An
    /// error (a 410 too) relists, which keeps the inventory right but leaves
    /// a gap in the timings: it is recorded, and latency is then invalid.
    async fn run(self: Arc<Self>, ctx: Arc<Ctx>, selector: String, mut rv: String) {
        let base = format!("/api/v1/namespaces/{}/pods", ctx.ns);
        loop {
            let q = format!(
                "{base}?watch=true&timeoutSeconds=20&allowWatchBookmarks=true&resourceVersion={}&labelSelector={}",
                enc(&rv),
                enc(&selector)
            );
            let r: Result<()> = async {
                let mut resp = ctx.kube.watch(&q).await?;
                let mut buf = Vec::new();
                loop {
                    // rustkube keeps a watch open past timeoutSeconds
                    // (rustkube#165), so the client's own timeout ends it.
                    // That loses nothing: the next watch resumes from the
                    // last whole event's resourceVersion. Not a gap.
                    let chunk = match resp.chunk().await {
                        Ok(Some(c)) => c,
                        Ok(None) => break,
                        Err(e) if e.is_timeout() => break,
                        Err(e) => return Err(e.into()),
                    };
                    buf.extend_from_slice(&chunk);
                    while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=i).collect();
                        if line.iter().all(u8::is_ascii_whitespace) {
                            continue;
                        }
                        let ev: Value = serde_json::from_slice(&line)?;
                        let obj = &ev["object"];
                        if ev["type"] == "ERROR" {
                            bail!("watch ERROR: {obj}");
                        }
                        if let Some(v) = obj["metadata"]["resourceVersion"].as_str() {
                            rv = v.to_string();
                        }
                        if ev["type"] != "BOOKMARK" {
                            self.observe(obj, ev["type"] == "DELETED");
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = r {
                eprintln!("turbomode watch: {e:#}; relisting");
                self.s.lock().unwrap().errors.push(format!("{e:#}"));
                match list(&ctx.kube, &base, Some(&selector)).await {
                    Ok((pods, v)) => {
                        pods.iter().for_each(|p| self.observe(p, false));
                        rv = v;
                    }
                    Err(e) => self.s.lock().unwrap().errors.push(format!("relist: {e:#}")),
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

// ---------------------------------------------------------------- run

struct Ctx {
    a: Args,
    kube: Client,
    /// stormblock's (the audit's).
    http: reqwest::Client,
    ns: String,
    image: String,
    host: Host,
    /// Every node of the cluster.
    nodes: Vec<String>,
    /// Resolved claim class, when the sqlite profile can run.
    storage_class: Option<Value>,
    out: Out,
    infra: AtomicUsize,
    /// `--image-file`'s length and digest at the start (None: not checked).
    image_digest: Option<(u64, String)>,
}

impl Ctx {
    fn could_not_run(&self, test: &str, took: Duration, why: impl Into<String>) {
        self.infra.fetch_add(1, Ordering::Relaxed);
        self.out.emit(Line::new(test, Status::Fail, took, format!("could not run: {}", why.into())));
    }
}

#[derive(Debug, Clone, Serialize)]
struct Created {
    name: String,
    uid: String,
    /// Seconds since the attempt began: the Pod POST issued, acknowledged.
    issued: f64,
    ack: f64,
    claim_issued: Option<f64>,
}

#[derive(Default)]
struct St {
    claims: Vec<Value>,
    created: Vec<Created>,
    pvs: BTreeMap<String, Value>,
    failures: Vec<Failure>,
    /// Volume ids the allocated audit saw.
    allocated_ids: Vec<String>,
    report: Map<String, Value>,
}

struct Attempt {
    ctx: Arc<Ctx>,
    profile: Profile,
    number: u32,
    token: String,
    selector: String,
    dir: PathBuf,
    expected: usize,
    obs: Arc<Observer>,
    st: Mutex<St>,
}

impl Attempt {
    /// A progress line on stderr. Results go to stdout only when a profile
    /// ends, so after a crash these are what shows how far it got (#26).
    fn note(&self, msg: &str) {
        eprintln!("turbomode {} attempt {} t+{:.0}s: {msg}", self.profile.name(), self.number, self.obs.t0.elapsed().as_secs_f64());
    }

    fn fail(&self, f: Failure) {
        self.st.lock().unwrap().failures.push(f);
    }

    fn set(&self, k: &str, v: Value) {
        self.st.lock().unwrap().report.insert(k.into(), v);
    }

    fn labels(&self) -> Value {
        let mut l = json!({"storm.io/test-run": self.ctx.a.run_id, "app.kubernetes.io/managed-by": MANAGED_BY});
        l[LABEL] = json!(self.token);
        l
    }

    fn report(&self) -> Value {
        let s = self.st.lock().unwrap();
        let mut r = s.report.clone();
        r.insert("run".into(), json!(self.token));
        r.insert("attempt".into(), json!(self.number));
        r.insert("profile".into(), json!(self.profile));
        r.insert("run_id".into(), json!(self.ctx.a.run_id));
        r.insert("namespace".into(), json!(self.ctx.ns));
        r.insert("image".into(), json!(self.ctx.image));
        r.insert("expected_pods".into(), json!(self.expected));
        r.insert("sleep_seconds".into(), json!(self.ctx.a.sleep_seconds));
        r.insert("nodes".into(), json!(self.ctx.nodes));
        r.insert("storage_class".into(), self.ctx.storage_class.clone().unwrap_or(Value::Null));
        r.insert("claims".into(), json!(s.claims));
        r.insert("pods".into(), json!(s.created));
        r.insert("pvs".into(), json!(s.pvs.values().collect::<Vec<_>>()));
        r.insert("failures".into(), json!(s.failures));
        Value::Object(r)
    }

    fn save(&self) {
        let _ = std::fs::write(self.dir.join("report.json"), serde_json::to_string_pretty(&self.report()).unwrap_or_default());
    }

    fn pods_path(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.ctx.ns)
    }

    fn claims_path(&self) -> String {
        format!("/api/v1/namespaces/{}/persistentvolumeclaims", self.ctx.ns)
    }

    fn pod_manifest(&self, name: &str) -> Value {
        let mut c = json!({
            "name": "work", "image": self.ctx.image, "imagePullPolicy": "IfNotPresent",
            "command": ["/test"], "args": ["sleep", self.ctx.a.sleep_seconds.to_string()],
        });
        let mut spec = json!({
            "restartPolicy": "Never", "automountServiceAccountToken": false,
            "terminationGracePeriodSeconds": 1,
        });
        if self.profile == Profile::Sqlite {
            c["args"] = json!(["sqlite"]);
            c["env"] = json!([
                {"name": "POD_UID", "valueFrom": {"fieldRef": {"fieldPath": "metadata.uid"}}},
                {"name": "SQLITE_SLEEP", "value": self.ctx.a.sleep_seconds.to_string()},
            ]);
            c["volumeMounts"] = json!([{"name": "data", "mountPath": "/data"}]);
            spec["volumes"] = json!([{"name": "data", "persistentVolumeClaim": {"claimName": name}}]);
        }
        spec["containers"] = json!([c]);
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": self.labels()}, "spec": spec})
    }

    fn claim_manifest(&self, name: &str) -> Value {
        let class = self.ctx.storage_class.as_ref().and_then(|c| c["metadata"]["name"].as_str()).unwrap_or_default();
        json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {"name": name, "labels": self.labels()},
            "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": class,
                     "resources": {"requests": {"storage": self.ctx.a.claim_size}}},
        })
    }

    async fn create(self: Arc<Self>, i: usize) -> Result<()> {
        let name = format!("{}-{i:04}", self.token);
        let kube = &self.ctx.kube;
        let mut claim_issued = None;
        if self.profile == Profile::Sqlite {
            let t = self.obs.t0.elapsed().as_secs_f64();
            let r = kube.post(&self.claims_path(), &self.claim_manifest(&name)).await.with_context(|| format!("claim {name}"))?;
            ensure!(r.ok(), "claim {name} answered {}: {}", r.code, r.body);
            self.st.lock().unwrap().claims.push(r.body);
            claim_issued = Some(t);
        }
        let issued = self.obs.t0.elapsed().as_secs_f64();
        let r = kube.post(&self.pods_path(), &self.pod_manifest(&name)).await.with_context(|| format!("pod {name}"))?;
        ensure!(r.ok(), "pod {name} answered {}: {}", r.code, r.body);
        let ack = self.obs.t0.elapsed().as_secs_f64();
        self.st.lock().unwrap().created.push(Created { name, uid: uid(&r.body), issued, ack, claim_issued });
        Ok(())
    }

    /// PVs bound to this attempt's claims (by claim UID, or name in the namespace).
    async fn capture_pvs(&self) -> Result<()> {
        let (pvs, _) = list(&self.ctx.kube, "/api/v1/persistentvolumes", None).await?;
        let mut s = self.st.lock().unwrap();
        let claims: BTreeSet<(String, String)> = s.claims.iter().map(|c| (uid(c), name(c))).collect();
        for pv in pvs {
            let r = &pv["spec"]["claimRef"];
            let mine = r["namespace"] == self.ctx.ns.as_str()
                && claims.iter().any(|(u, n)| r["uid"] == u.as_str() || r["name"] == n.as_str());
            if mine {
                s.pvs.insert(uid(&pv), pv);
            }
        }
        Ok(())
    }

    /// One storage audit: evidence in `storage-<phase>.json`.
    async fn audit(&self, phase: Phase) -> Result<(), Failure> {
        self.save();
        let ctx = &self.ctx;
        let here = ctx.a.node_name.clone().unwrap_or_default();
        if ctx.nodes != [here.clone()] {
            return Err(Failure::new(Kind::Storage, format!("the audit sees only node {here:?}; the cluster has {:?}", ctx.nodes)));
        }
        let req = {
            let s = self.st.lock().unwrap();
            let names = s.claims.iter().map(|c| format!("pvc-{}-{}", ctx.ns, name(c))).collect();
            let mut uids: BTreeSet<String> = s.created.iter().map(|c| c.uid.clone()).collect();
            uids.extend(self.obs.s.lock().unwrap().objects.keys().cloned());
            // PV handles keep identities even when a run never reached its
            // allocated audit; names also find partly provisioned claims.
            let mut ids = s.allocated_ids.clone();
            ids.extend(s.pvs.values().filter_map(|p| p["spec"]["csi"]["volumeHandle"].as_str().map(str::to_string)));
            Request { names, uids: uids.into_iter().collect(), ids }
        };
        let ev = turbo_audit::collect(&ctx.http, &ctx.host, &req)
            .await
            .map_err(|e| Failure::new(Kind::Storage, format!("backend {} audit: {e:#}", phase.name())))?;
        let ok = turbo_audit::verified(phase, &ev, &req.names);
        if phase == Phase::Allocated {
            self.st.lock().unwrap().allocated_ids = turbo_audit::volume_ids(&ev);
        }
        let doc = json!({"verified": ok, "phase": phase.name(), "node": here, "evidence": ev});
        let _ = std::fs::write(self.dir.join(format!("storage-{}.json", phase.name())), serde_json::to_string_pretty(&doc).unwrap_or_default());
        if !ok {
            return Err(Failure::new(Kind::Storage, format!("backend {} audit did not verify storage", phase.name())));
        }
        Ok(())
    }

    /// The workload: create, wait for every Pod to finish, check the evidence.
    async fn body(self: &Arc<Self>) -> Result<(), Failure> {
        let ctx = &self.ctx;
        if self.profile == Profile::Sqlite {
            self.audit(Phase::Before).await?;
        }
        let gate = Arc::new(Semaphore::new(ctx.a.concurrency.max(1)));
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..self.expected {
            let (me, gate) = (self.clone(), gate.clone());
            tasks.spawn(async move {
                let _permit = gate.acquire_owned().await;
                me.create(i).await
            });
        }
        let mut create_errors = 0;
        while let Some(r) = tasks.join_next().await {
            let r = r.map_err(|e| anyhow::anyhow!("create task: {e}")).and_then(|r| r);
            if let Err(e) = r {
                create_errors += 1;
                self.fail(Failure::new(Kind::Transient, format!("create: {e:#}")));
            }
        }
        self.save();
        self.note(&format!("{} creates issued, {create_errors} failed", self.expected));
        if create_errors > 0 {
            // A lost acknowledgement may have committed: cleanup finds it by label.
            return Err(Failure::new(Kind::Transient, format!("{create_errors}/{} creates failed", self.expected)));
        }
        let deadline = Instant::now() + Duration::from_secs(ctx.a.finish_timeout);
        let mut noted = Instant::now();
        let finished: Vec<Value> = loop {
            let done: Vec<Value> = {
                let s = self.obs.s.lock().unwrap();
                s.objects.values().filter(|p| matches!(p["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed"))).cloned().collect()
            };
            if done.len() >= self.expected {
                break done;
            }
            if Instant::now() >= deadline {
                self.note(&format!("finish timeout: {}/{} Pods finished", done.len(), self.expected));
                return Err(Failure::new(Kind::Transient, format!("partial startup: only {}/{} Pods finished", done.len(), self.expected)));
            }
            if noted.elapsed() >= Duration::from_secs(60) {
                noted = Instant::now();
                self.note(&format!("{}/{} Pods finished", done.len(), self.expected));
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), self.obs.changed.notified()).await;
        };
        let mut bad = 0;
        for pod in &finished {
            let (n, u) = (name(pod), uid(pod));
            let phase = pod["status"]["phase"].as_str().unwrap_or_default();
            if self.profile == Profile::Sleep {
                if phase != "Succeeded" {
                    self.fail(Failure::new(Kind::Transient, format!("Pod {n} ({u}) {phase}")));
                }
                continue;
            }
            // Keep every log, failed Pods' too: it is the corruption evidence.
            let log = match ctx.kube.text(&format!("{}/{n}/log", self.pods_path())).await {
                Ok((c, t)) if (200..300).contains(&c) => t,
                other => {
                    let why = match other {
                        Ok((c, t)) => format!("answered {c}: {}", t.chars().take(300).collect::<String>()),
                        Err(e) => format!("{e:#}"),
                    };
                    bad += 1;
                    self.fail(Failure::new(Kind::Integrity, format!("SQLite log {n} ({u}) unreadable: {why}")));
                    continue;
                }
            };
            let _ = std::fs::write(self.dir.join(format!("{u}.log")), &log);
            let mut problem = sqlite::evidence_problem(&log, &u, ctx.a.sleep_seconds);
            if phase != "Succeeded" {
                problem = Some(format!("Pod {phase}{}", problem.map(|p| format!(", {p}")).unwrap_or_default()));
            }
            if let Some(p) = problem {
                bad += 1;
                self.fail(Failure::new(Kind::Integrity, format!("SQLite evidence {n} ({u}): {p}")));
            }
        }
        if bad > 0 {
            return Err(Failure::new(Kind::Integrity, format!("{bad}/{} Pods without valid SQLite evidence", self.expected)));
        }
        if self.profile == Profile::Sqlite {
            self.capture_pvs().await.map_err(error)?;
            let n = self.st.lock().unwrap().pvs.len();
            if n != self.expected {
                return Err(Failure::new(Kind::Storage, format!("expected {} distinct PVs, found {n}", self.expected)));
            }
            self.audit(Phase::Allocated).await?;
        }
        Ok(())
    }

    /// Timings from the observer, into the report.
    fn timings(&self) {
        // Lock order everywhere: st, then the observer's.
        let created = self.st.lock().unwrap().created.clone();
        let o = self.obs.s.lock().unwrap();
        let from = |phase: &str, start: &dyn Fn(&Created) -> Option<f64>| {
            percentiles(created.iter().filter_map(|c| Some(o.records.get(&c.uid)?.get(phase)? - start(c)?)).collect())
        };
        let seconds = json!({
            "create_ack": percentiles(created.iter().map(|c| c.ack - c.issued).collect()),
            "request_to_scheduled": from("scheduled", &|c: &Created| Some(c.issued)),
            "request_to_running": from("running", &|c: &Created| Some(c.issued)),
            "request_to_finished": from("finished", &|c: &Created| Some(c.issued)),
            "claim_request_to_running": from("running", &|c: &Created| c.claim_issued),
        });
        let (errors, peak, records, finals) = (o.errors.clone(), o.peak, json!(o.records), json!(o.objects.values().collect::<Vec<_>>()));
        drop(o);
        let _ = std::fs::write(self.dir.join("pods.json"), serde_json::to_string(&finals).unwrap_or_default());
        self.set("latency_valid", json!(errors.is_empty()));
        self.set("watch_errors", json!(errors));
        self.set("peak_running_observed", json!(peak));
        self.set("observations", records);
        self.set("seconds", seconds);
    }

    /// Delete exactly this attempt's Pods and claims and wait until they,
    /// their PVs and attachments are gone, then (sqlite) the after audit.
    async fn cleanup(&self) -> Result<(), Failure> {
        let ctx = &self.ctx;
        let kube = &ctx.kube;
        let start = Instant::now();
        let left = |e: anyhow::Error| Failure::new(Kind::Cleanup, format!("{e:#}"));
        let mut pods: BTreeMap<String, String> = self.st.lock().unwrap().created.iter().map(|c| (c.uid.clone(), c.name.clone())).collect();
        let (listed, _) = list(kube, &self.pods_path(), Some(&self.selector)).await.map_err(left)?;
        self.note(&format!("cleanup: {} Pods listed", listed.len()));
        pods.extend(listed.iter().map(|p| (uid(p), name(p))));
        // A claim POST can commit with its ack lost: recover claims by label too.
        let (listed, _) = list(kube, &self.claims_path(), Some(&self.selector)).await.map_err(left)?;
        let claims: BTreeMap<String, String> = {
            let mut s = self.st.lock().unwrap();
            let known: BTreeSet<String> = s.claims.iter().map(uid).collect();
            s.claims.extend(listed.into_iter().filter(|c| !known.contains(&uid(c))));
            s.claims.iter().map(|c| (uid(c), name(c))).collect()
        };
        self.note(&format!("cleanup: {} claims listed", claims.len()));
        // Before the deletes: the PVs' handles are what the after audit looks for.
        self.capture_pvs().await.map_err(left)?;
        self.save();
        self.note(&format!("cleanup: deleting {} Pods and {} claims", pods.len(), claims.len()));
        for (u, n) in &pods {
            match kube.delete_uid(&format!("{}/{n}", self.pods_path()), u).await {
                Ok(r) if r.ok() || r.code == 404 => {}
                Ok(r) => self.fail(Failure::new(Kind::Cleanup, format!("delete pod {n}: {} {}", r.code, r.body))),
                Err(e) => self.fail(Failure::new(Kind::Cleanup, format!("delete pod {n}: {e:#}"))),
            }
        }
        for (u, n) in &claims {
            match kube.delete_uid(&format!("{}/{n}", self.claims_path()), u).await {
                Ok(r) if r.ok() || r.code == 404 => {}
                Ok(r) => self.fail(Failure::new(Kind::Cleanup, format!("delete claim {n}: {} {}", r.code, r.body))),
                Err(e) => self.fail(Failure::new(Kind::Cleanup, format!("delete claim {n}: {e:#}"))),
            }
        }
        let deadline = start + Duration::from_secs(ctx.a.cleanup_timeout);
        loop {
            let (p, _) = list(kube, &self.pods_path(), Some(&self.selector)).await.map_err(left)?;
            let (c, _) = list(kube, &self.claims_path(), Some(&self.selector)).await.map_err(left)?;
            self.capture_pvs().await.map_err(left)?;
            let (all_pvs, _) = list(kube, "/api/v1/persistentvolumes", None).await.map_err(left)?;
            let known: BTreeSet<String> = self.st.lock().unwrap().pvs.keys().cloned().collect();
            let pvs: Vec<Value> = all_pvs.into_iter().filter(|v| known.contains(&uid(v))).collect();
            let pv_names: BTreeSet<String> = self.st.lock().unwrap().pvs.values().map(name).collect();
            let (att, _) = list(kube, "/apis/storage.k8s.io/v1/volumeattachments", None).await.map_err(left)?;
            let att: Vec<Value> = att
                .into_iter()
                .filter(|a| a["spec"]["source"]["persistentVolumeName"].as_str().is_some_and(|n| pv_names.contains(n)))
                .collect();
            let empty = p.is_empty() && c.is_empty() && pvs.is_empty() && att.is_empty();
            self.set("residue", json!({"pods": p, "claims": c, "pvs": pvs, "attachments": att}));
            if empty {
                break;
            }
            if Instant::now() >= deadline {
                self.note(&format!("cleanup timeout: {} Pods, {} claims, {} PVs, {} attachments left", p.len(), c.len(), pvs.len(), att.len()));
                return Err(Failure::new(Kind::Cleanup, "cleanup left resources; see residue in report.json"));
            }
            tokio::time::sleep(Duration::from_secs(1)).await; // bounded observation, not a reconciler
        }
        self.set("cleanup_api_seconds", json!(start.elapsed().as_secs_f64()));
        if self.profile == Profile::Sqlite {
            loop {
                match self.audit(Phase::After).await {
                    Ok(()) => break,
                    Err(f) if Instant::now() >= deadline => return Err(f),
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        }
        self.set("cleanup_seconds", json!(start.elapsed().as_secs_f64()));
        self.note("cleanup verified");
        Ok(())
    }
}

fn token() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    format!("tm{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// One attempt, start to verified (or not) cleanup. Its report and failures.
async fn attempt(ctx: &Arc<Ctx>, profile: Profile, number: u32, dir: PathBuf) -> (Value, Vec<Failure>) {
    let _ = std::fs::create_dir_all(&dir);
    let token = token();
    let expected = match profile {
        Profile::Sleep => ctx.a.sleep_pods,
        Profile::Sqlite => ctx.a.sqlite_pods,
    };
    let at = Arc::new(Attempt {
        ctx: ctx.clone(),
        profile,
        number,
        selector: format!("{LABEL}={token}"),
        token,
        dir,
        expected,
        obs: Arc::new(Observer { s: Mutex::new(ObsState::default()), changed: Notify::new(), t0: Instant::now() }),
        st: Mutex::new(St::default()),
    });
    at.set("started", json!(now_unix()));
    at.note(&format!("start: {expected} Pods, run label {}", at.token));
    let mut watch = None;
    let body = async {
        let (_, rv) = list(&ctx.kube, &at.pods_path(), Some(&at.selector)).await.map_err(error)?;
        watch = Some(tokio::spawn(at.obs.clone().run(ctx.clone(), at.selector.clone(), rv)));
        at.body().await
    }
    .await;
    if let Err(f) = body {
        at.note(&format!("{:?}: {}", f.kind, f.error));
        at.fail(f);
    }
    if let Some(w) = watch {
        w.abort();
    }
    at.timings();
    at.note("timings recorded");
    at.save();
    at.note("report saved; cleanup");
    let cleanup = at.cleanup().await;
    let verified = cleanup.is_ok();
    if let Err(f) = cleanup {
        at.fail(f);
    }
    if let Some(first) = &ctx.image_digest {
        let now = image_digest(&ctx.a.image_file).unwrap_or_else(|e| (0, format!("unreadable: {e:#}")));
        at.set("image_digest", json!({"start": first.1, "now": now.1}));
        if &now != first {
            at.note("the test image's own file changed on its volume");
            at.fail(Failure::new(Kind::Integrity, format!(
                "{} changed on the image's volume since the start: {} bytes sha256 {} → {} bytes sha256 {} (stormblock#267)",
                ctx.a.image_file.display(), first.0, first.1, now.0, now.1)));
        }
    }
    at.set("cleanup_verified", json!(verified));
    let (passed, n) = {
        let s = at.st.lock().unwrap();
        (s.failures.is_empty() && s.created.len() == expected && verified, s.created.len())
    };
    at.set("created_pods", json!(n));
    at.set("passed", json!(passed));
    at.note(if passed { "passed" } else { "failed" });
    at.set("finished", json!(now_unix()));
    at.save();
    let failures = at.st.lock().unwrap().failures.clone();
    (at.report(), failures)
}

/// A profile's attempts, its summary and its line.
async fn profile(ctx: &Arc<Ctx>, profile: Profile, began: Instant) {
    let test = format!("turbomode/{}", profile.name());
    let t = Instant::now();
    let root = ctx.a.results.join("turbomode").join(profile.name());
    let mut summary = json!({"profile": profile, "run_id": ctx.a.run_id, "image": ctx.image,
        "max_attempts": ctx.a.attempts, "attempts": [], "passed": false});
    let save = |s: &Value| {
        let _ = std::fs::create_dir_all(&root);
        let _ = std::fs::write(root.join("summary.json"), serde_json::to_string_pretty(s).unwrap_or_default());
    };
    for number in 1..=ctx.a.attempts {
        let (r, failures) = attempt(ctx, profile, number, attempt_dir(&ctx.a.results, profile, number)).await;
        let (passed, clean) = (r["passed"] == true, r["cleanup_verified"] == true);
        summary["attempts"].as_array_mut().unwrap().push(json!({
            "attempt": number, "run": r["run"], "passed": passed, "cleanup_verified": clean,
            "failures": r["failures"], "latency_valid": r["latency_valid"],
            "peak_running_observed": r["peak_running_observed"], "seconds": r["seconds"],
            "cleanup_seconds": r["cleanup_seconds"], "report": format!("attempt-{number}/report.json"),
        }));
        if passed {
            summary["passed"] = json!(true);
            summary["passed_on_attempt"] = json!(number);
            break;
        }
        if !retryable(passed, clean, &failures) {
            let mut kinds: Vec<String> = failures.iter().map(|f| json!(f.kind).as_str().unwrap_or_default().to_string()).collect();
            kinds.sort();
            kinds.dedup();
            summary["stopped"] = json!(if clean { format!("final failure: {}", kinds.join(", ")) } else { "cleanup not verified".into() });
            break;
        }
        save(&summary);
        if number == ctx.a.attempts {
            summary["stopped"] = json!(format!("all {number} attempts failed"));
        } else if !attempt_fits(began.elapsed() + Duration::from_secs(ctx.a.retry_delay), ctx.a.timeout, ctx.a.finish_timeout, ctx.a.cleanup_timeout) {
            summary["stopped"] = json!("run window (STORM_TIMEOUT) too short for another attempt");
            break;
        } else {
            tokio::time::sleep(Duration::from_secs(ctx.a.retry_delay)).await;
        }
    }
    // A pass after retries is still reported with every failed attempt beside it.
    let attempts = summary["attempts"].as_array().map_or(0, Vec::len);
    summary["retried"] = json!(attempts > 1);
    save(&summary);
    let last = &summary["attempts"][attempts.saturating_sub(1)];
    let s = &last["seconds"];
    let detail = format!(
        "{} on attempt {attempts} of {}{}; {} Pods, peak running {}; request→running p50 {} p95 {} max {} s{}; cleanup {} s; summary {}",
        if summary["passed"] == true { "passed" } else { "failed" },
        ctx.a.attempts,
        summary["stopped"].as_str().map(|w| format!(" ({w})")).unwrap_or_default(),
        match profile {
            Profile::Sleep => ctx.a.sleep_pods,
            Profile::Sqlite => ctx.a.sqlite_pods,
        },
        last["peak_running_observed"],
        s["request_to_running"]["p50"],
        s["request_to_running"]["p95"],
        s["request_to_running"]["max"],
        if last["latency_valid"] == false { " (watch gap: latency invalid)" } else { "" },
        last["cleanup_seconds"],
        root.join("summary.json").display(),
    );
    let mut line = Line::new(test, if summary["passed"] == true { Status::Pass } else { Status::Fail }, t.elapsed(), detail);
    line.wave = Some(summary["attempts"].clone());
    ctx.out.emit(line);
}

/// The default StorageClass, or the named one.
async fn storage_class(kube: &Client, named: Option<&str>) -> Result<Result<Value, String>> {
    if let Some(n) = named {
        let r = kube.get(&format!("/apis/storage.k8s.io/v1/storageclasses/{n}")).await?;
        ensure!(r.ok(), "StorageClass {n} answered {}: {}", r.code, r.body);
        return Ok(Ok(r.body));
    }
    let (all, _) = list(kube, "/apis/storage.k8s.io/v1/storageclasses", None).await?;
    let defaults: Vec<Value> = all
        .into_iter()
        .filter(|c| c["metadata"]["annotations"]["storageclass.kubernetes.io/is-default-class"] == "true")
        .collect();
    match defaults.len() {
        1 => Ok(Ok(defaults[0].clone())),
        n => Ok(Err(format!("{n} default StorageClasses; pass --storage-class"))),
    }
}

/// Cluster-scoped reads the suite needs beyond the run namespace
/// (requires.toml `[turbomode]` cluster_read). A 403 is "could not run".
const CLUSTER_READS: [&str; 3] = ["/api/v1/nodes", "/api/v1/persistentvolumes", "/apis/storage.k8s.io/v1/volumeattachments"];

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "turbomode");
    match run(a, out).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("test turbomode: {e:#}");
            2
        }
    }
}

pub(crate) async fn run(a: Args, out: Out) -> Result<i32> {
    let began = Instant::now();
    if !(1..=MAX_ATTEMPTS).contains(&a.attempts) {
        bail!("--attempts must be 1..{MAX_ATTEMPTS}");
    }
    let t = Instant::now();
    let kube = Client::new(&a.api, a.token_file.as_deref(), a.insecure).await?;
    let ns = match &a.namespace {
        Some(n) => n.clone(),
        None => tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
            .await
            .context("no --namespace / STORM_NAMESPACE and no service-account namespace")?
            .trim()
            .to_string(),
    };
    let fail_all = |out: &Out, why: String| -> Result<i32> {
        out.emit(Line::new("turbomode/preflight", Status::Fail, t.elapsed(), format!("could not run: {why}")));
        out.summary();
        Ok(2)
    };
    let image = match &a.image {
        Some(i) => i.clone(),
        None => {
            let r = kube.get(&format!("/api/v1/namespaces/{ns}/pods")).await?;
            match long::own_image(&kube::items(&r.body), &std::env::var("HOSTNAME").unwrap_or_default()) {
                Some(i) => i,
                None => return fail_all(&out, format!("cannot learn this test's image from its pod in {ns} ({}); pass --image", r.code)),
            }
        }
    };
    let mut nodes = Vec::new();
    for path in CLUSTER_READS {
        let r = kube.get(&format!("{path}?limit=1")).await?;
        if !r.ok() {
            return fail_all(
                &out,
                format!("{path} answered {}: the suite needs cluster read of nodes, persistentvolumes and volumeattachments (requires.toml [turbomode], stormcentral#55)", r.code),
            );
        }
    }
    for n in list(&kube, "/api/v1/nodes", None).await?.0 {
        nodes.push(name(&n));
    }
    nodes.sort();
    let host = host_of(&a);
    // What the sqlite profile needs that the sleeping one does not.
    let mut sqlite_blocked = None;
    let mut class = None;
    if a.profiles.contains(&Profile::Sqlite) {
        sqlite_blocked = match storage_class(&kube, a.storage_class.as_deref()).await {
            Err(e) => Some(format!("{e:#}")),
            Ok(Err(why)) => Some(why),
            Ok(Ok(c)) if c["reclaimPolicy"].as_str().unwrap_or("Delete") != "Delete" => Some(format!(
                "StorageClass {} reclaims with {}: the test needs Delete and will not change a shared class",
                name(&c),
                c["reclaimPolicy"]
            )),
            Ok(Ok(c)) => {
                class = Some(c);
                None
            }
        };
        if sqlite_blocked.is_none() {
            sqlite_blocked = match &a.node_name {
                None => Some("TURBOMODE_NODE (downward API spec.nodeName) is not set".into()),
                Some(here) if nodes != [here.clone()] => {
                    Some(format!("the storage audit sees only node {here}; the cluster has {nodes:?} (an incomplete inventory)"))
                }
                Some(_) => host.unusable().map(|w| format!("{w} (the Job's host access, stormcentral#74)")),
            };
        }
    }
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let image_digest = a.image_file.is_file().then(|| image_digest(&a.image_file).ok()).flatten();
    let ctx = Arc::new(Ctx {
        a,
        kube,
        http,
        ns,
        image,
        host,
        nodes,
        storage_class: class,
        out,
        infra: AtomicUsize::new(0),
        image_digest,
    });
    for p in ctx.a.profiles.clone() {
        if p == Profile::Sqlite
            && let Some(why) = &sqlite_blocked
        {
            ctx.could_not_run("turbomode/sqlite", t.elapsed(), why.clone());
            continue;
        }
        if !attempt_fits(began.elapsed(), ctx.a.timeout, ctx.a.finish_timeout, ctx.a.cleanup_timeout) {
            ctx.could_not_run(&format!("turbomode/{}", p.name()), t.elapsed(), "run window (STORM_TIMEOUT) too short for an attempt");
            continue;
        }
        profile(&ctx, p, began).await;
    }
    ctx.out.summary();
    let infra = ctx.infra.load(Ordering::Relaxed);
    Ok(if ctx.out.failed() > infra {
        1
    } else if infra > 0 {
        2
    } else {
        0
    })
}

/// Where an attempt's evidence lands.
pub fn attempt_dir(results: &Path, profile: Profile, n: u32) -> PathBuf {
    results.join("turbomode").join(profile.name()).join(format!("attempt-{n}"))
}

/// The host's paths: given ones as they are, the rest under the runner's
/// read-only mounts (`STORM_HOST_ROOT`, stormcentral#74).
fn host_of(a: &Args) -> Host {
    let under = |given: &Option<PathBuf>, path: &str| given.clone().unwrap_or_else(|| a.host_root.join(path));
    let proc_root = under(&a.proc_root, "proc");
    Host {
        host_mountinfo: a.host_mountinfo.clone().unwrap_or_else(|| proc_root.join("1/mountinfo")),
        cgroup_root: under(&a.cgroup_root, "sys/fs/cgroup"),
        token_file: under(&a.stormblock_token, "run/stormblock/engine/api_token"),
        proc_root,
        stormblock: a.stormblock_url.clone().unwrap_or_else(|| format!("http://{}:9090", a.node)),
        kubelet_pods: a.kubelet_pods.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(kind: Kind) -> Failure {
        Failure::new(kind, "x")
    }

    #[test]
    fn an_attempt_starts_only_if_its_worst_case_fits() {
        // Night: 14400 s window, 3600 s finish, 600 s cleanup, 120 s reserve.
        assert!(attempt_fits(Duration::ZERO, 14400, 3600, 600));
        assert!(attempt_fits(Duration::from_secs(10080), 14400, 3600, 600), "exactly fits");
        assert!(!attempt_fits(Duration::from_secs(10081), 14400, 3600, 600), "would overrun the Job deadline");
        assert!(!attempt_fits(Duration::ZERO, 4000, 3600, 600), "a window shorter than one attempt");
        // Day (#43): 900 s window, 240 + 120 + 120 = 480 s an attempt, so
        // sqlite still starts after a sleep profile of up to 420 s.
        assert!(attempt_fits(Duration::from_secs(420), 900, 240, 120));
        assert!(!attempt_fits(Duration::from_secs(421), 900, 240, 120));
    }

    #[test]
    fn day_and_night_sizes() {
        let base = |extra: &[&str]| ["test"].iter().chain(extra).map(|s| s.to_string()).collect::<Vec<_>>();
        let day = Args::try_parse_from(argv(base(&[]), false)).unwrap();
        assert_eq!((day.sleep_pods, day.sqlite_pods, day.sleep_seconds, day.attempts), (100, 25, 60, 2));
        assert_eq!((day.finish_timeout, day.cleanup_timeout), (240, 120));
        let night = Args::try_parse_from(argv(base(&[]), true)).unwrap();
        assert_eq!((night.sleep_pods, night.sqlite_pods, night.sleep_seconds, night.attempts), (1000, 100, 120, 3));
        assert_eq!((night.finish_timeout, night.cleanup_timeout), (3600, 600));
        // A flag given to turbomode-night wins over its preset.
        let n = Args::try_parse_from(argv(base(&["--sleep-pods", "7", "--attempts=1"]), true)).unwrap();
        assert_eq!((n.sleep_pods, n.attempts, n.sqlite_pods), (7, 1, 100));
    }

    #[tokio::test]
    async fn a_watch_cut_by_the_client_timeout_reads_as_a_timeout() {
        // What the observer treats as a normal end of a watch (rustkube#165):
        // the headers and an event arrive, then the stream stalls.
        use tokio::io::AsyncWriteExt;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/w", l.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\n{}\n\r\n").await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let c = reqwest::Client::builder().timeout(Duration::from_millis(500)).build().unwrap();
        let mut r = c.get(url).send().await.unwrap();
        assert_eq!(&r.chunk().await.unwrap().unwrap()[..], b"{}\n");
        let e = r.chunk().await.unwrap_err();
        assert!(e.is_timeout(), "{e:#}");
    }

    #[test]
    fn the_image_digest_reads_the_file_and_sees_a_change() {
        let p = std::env::temp_dir().join(format!("qa-image-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        let first = image_digest(&p).unwrap();
        assert_eq!(first, (3, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string()));
        std::fs::write(&p, b"abd").unwrap();
        assert_ne!(image_digest(&p).unwrap(), first);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn host_paths_follow_the_runners_mounts() {
        let a = Args::try_parse_from(["turbomode", "--host-root", "/host", "--node", "10.0.0.2"]).unwrap();
        let h = host_of(&a);
        assert_eq!(h.proc_root, Path::new("/host/proc"));
        assert_eq!(h.host_mountinfo, Path::new("/host/proc/1/mountinfo"));
        assert_eq!(h.cgroup_root, Path::new("/host/sys/fs/cgroup"));
        assert_eq!(h.token_file, Path::new("/host/run/stormblock/engine/api_token"));
        assert_eq!(h.stormblock, "http://10.0.0.2:9090");
        let a = Args::try_parse_from(["turbomode", "--host-root", "/host", "--proc-root", "/p", "--cgroup-root", "/c"]).unwrap();
        let h = host_of(&a);
        assert_eq!((h.proc_root.as_path(), h.host_mountinfo.as_path()), (Path::new("/p"), Path::new("/p/1/mountinfo")));
        assert_eq!(h.cgroup_root, Path::new("/c"), "a given path is kept as it is");
    }

    #[test]
    fn only_clean_transient_failures_retry() {
        assert!(retryable(false, true, &[f(Kind::Transient), f(Kind::Transient)]));
        assert!(!retryable(false, false, &[f(Kind::Transient)]), "cleanup not verified");
        assert!(!retryable(false, true, &[f(Kind::Transient), f(Kind::Integrity)]), "corruption is final");
        assert!(!retryable(false, true, &[f(Kind::Storage)]));
        assert!(!retryable(false, true, &[f(Kind::Cleanup)]));
        assert!(!retryable(false, true, &[f(Kind::Error)]));
        assert!(!retryable(false, true, &[]), "a failure with no reason is not transient");
        assert!(!retryable(true, true, &[]));
    }

    #[test]
    fn nearest_rank_percentiles() {
        let p = percentiles((1..=100).map(f64::from).collect());
        assert_eq!(p["p50"], 50.0);
        assert_eq!(p["p95"], 95.0);
        assert_eq!(p["p99"], 99.0);
        assert_eq!(p["max"], 100.0);
        assert_eq!(percentiles(vec![]), json!({"samples": 0}));
        assert_eq!(percentiles(vec![3.0])["p50"], 3.0);
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(enc("qa.storm.io/turbomode-run=tm01"), "qa.storm.io%2Fturbomode-run%3Dtm01");
        assert_eq!(enc("a+b/c=="), "a%2Bb%2Fc%3D%3D");
    }

    #[test]
    fn evidence_lands_per_attempt() {
        assert_eq!(attempt_dir(Path::new("/results"), Profile::Sqlite, 2), PathBuf::from("/results/turbomode/sqlite/attempt-2"));
    }

    #[test]
    fn the_observer_tracks_phases_and_peak() {
        let o = Observer { s: Mutex::new(ObsState::default()), changed: Notify::new(), t0: Instant::now() };
        let pod = |u: &str, phase: &str| json!({"metadata": {"uid": u}, "spec": {"nodeName": "n"}, "status": {"phase": phase}});
        o.observe(&pod("a", "Running"), false);
        o.observe(&pod("b", "Running"), false);
        o.observe(&pod("a", "Succeeded"), false);
        o.observe(&pod("b", "Running"), true);
        let s = o.s.lock().unwrap();
        assert_eq!(s.peak, 2);
        assert!(s.records["a"].contains_key("finished"));
        assert!(s.records["b"].contains_key("deleted"));
        assert!(!s.records["b"].contains_key("finished"));
    }
}
