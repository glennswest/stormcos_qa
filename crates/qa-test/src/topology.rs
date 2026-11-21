//! `/test three-master` and `/test full-six` — the multi-node topologies
//! (#3). Each declares `topology` in `test/requires.toml` (stormcentral#290):
//! the runner starts it only on a machine whose record says that topology
//! (a single machine is a skip before any build), and passes
//! `STORM_TOPOLOGY`, `STORM_MASTERS` and `STORM_NODES`.
//!
//! Roles come from stormcluster's own objects (`cluster.storm.io/v1alpha1`,
//! cluster-scoped): the `Cluster` (phase, masters, workers) and each
//! `ClusterMember` (role, address, phase).
//!
//! `three-master`:
//! - `topology/cluster`: one Cluster, `Ready`, with the masters asked for;
//! - `topology/masters`: every master member `Ready`, and its Node Ready;
//! - `apiserver/<master>`: that master's own apiserver answers `/readyz`
//!   (`https://<address>:6443`, this run's token, the cluster CA);
//! - `etcd/replicated`: a ConfigMap written through one master is read back
//!   through every other (one store behind all of them);
//! - `quorum/lose-one`: skip — stopping a master is the runner's
//!   (stormcentral#1161).
//!
//! `full-six` adds the workers:
//! - `topology/workers`: the workers asked for, `Ready`, Nodes Ready;
//! - `schedule/workers`: a `/test serve` pod pinned to each worker (by a
//!   `nodeSelector` on its Node's `kubernetes.io/hostname` label, through
//!   the scheduler) runs there, on that Node by name; a Ready worker whose
//!   Node has no such label is a failure naming it;
//! - `cni/cross-node`: an agent pod on the first worker reaches the serve
//!   pods on the others by pod IP;
//! - `csi/reattach`: a claim written by `/test claim` on one worker is read
//!   back intact by a pod on another worker. Each claim pod must be seen
//!   gone (404) before the next starts and before a pass; a refused delete
//!   or a pod still there at the timeout fails it, and cleanup that fails
//!   is reported;
//! - `drain/reboot`: skip — node writes and reboots are the runner's
//!   (stormcentral#1161).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use serde_json::{Value, json};

use crate::agent::{Plan, Probe, SERVE_PORT, Target};
use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const FAULTS: &str = "stormcentral#1161";

#[derive(Parser, Debug)]
#[command(name = "test three-master|full-six", about = "Multi-node topology checks (#3)")]
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
    /// Masters the cluster must have (the runner's STORM_MASTERS).
    #[arg(long, env = "STORM_MASTERS", default_value_t = 3)]
    masters: usize,
    /// Nodes in all (the runner's STORM_NODES); workers = nodes − masters.
    #[arg(long, env = "STORM_NODES", default_value_t = 3)]
    nodes: usize,
    /// How long a pod may take to run, or a claim to be written or read.
    #[arg(long, default_value_t = 180)]
    pod_timeout: u64,
    /// The claim's StorageClass (default: the cluster's default class).
    #[arg(long)]
    storage_class: Option<String>,
    /// This test's image (default: the Job pod's own).
    #[arg(long)]
    image: Option<String>,
}

/// A cluster member as the checks see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub address: String,
    pub phase: String,
    /// The Node it is, if one is found (by name, else by address).
    pub node: Option<String>,
    /// That Node's `kubernetes.io/hostname` label, what a nodeSelector pins
    /// on; it need not equal the Node's name (finding 366cb39723c7).
    pub hostname: Option<String>,
    pub node_ready: bool,
}

/// A worker a pod is placed on: `host` for the nodeSelector, `node` to
/// check the pod's `spec.nodeName` against.
#[derive(Debug, Clone, PartialEq)]
pub struct Worker {
    pub node: String,
    pub host: String,
}

/// The Ready workers to place pods on (at most three), and the Nodes of
/// Ready workers that have no `kubernetes.io/hostname` label, which no
/// nodeSelector can pin.
pub fn placement(ms: &[Member]) -> (Vec<Worker>, Vec<String>) {
    let mut unlabelled = Vec::new();
    let mut workers = Vec::new();
    for m in ms.iter().filter(|m| m.role == "worker" && m.node_ready) {
        let Some(node) = &m.node else { continue };
        match &m.hostname {
            Some(host) => workers.push(Worker { node: node.clone(), host: host.clone() }),
            None => unlabelled.push(node.clone()),
        }
    }
    workers.truncate(3);
    (workers, unlabelled)
}

fn node_ready(n: &Value) -> bool {
    n["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

fn internal_ip(n: &Value) -> Option<&str> {
    n["status"]["addresses"].as_array()?.iter().find(|a| a["type"] == "InternalIP").and_then(|a| a["address"].as_str())
}

/// Every ClusterMember, matched to its Node.
pub fn members(cms: &[Value], nodes: &[Value]) -> Vec<Member> {
    let mut v: Vec<Member> = cms
        .iter()
        .map(|m| {
            let name = m["metadata"]["name"].as_str().unwrap_or("?").to_string();
            let role = m["status"]["role"].as_str().or(m["spec"]["role"].as_str()).unwrap_or("?").to_string();
            let address = m["status"]["address"].as_str().unwrap_or("").to_string();
            let node = nodes
                .iter()
                .find(|n| n["metadata"]["name"].as_str() == Some(name.as_str()))
                .or_else(|| nodes.iter().find(|n| !address.is_empty() && internal_ip(n) == Some(address.as_str())));
            Member {
                address: if address.is_empty() { node.and_then(internal_ip).unwrap_or("").to_string() } else { address },
                phase: m["status"]["phase"].as_str().unwrap_or("?").to_string(),
                node: node.and_then(|n| n["metadata"]["name"].as_str()).map(str::to_string),
                hostname: node.and_then(|n| n["metadata"]["labels"]["kubernetes.io/hostname"].as_str()).map(str::to_string),
                node_ready: node.is_some_and(node_ready),
                name,
                role,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// `topology/cluster`: exactly one Cluster, `Ready`, with at least the
/// masters (and workers) asked for.
pub fn judge_cluster(clusters: &[Value], masters: usize, workers: usize) -> (Status, String) {
    let [c] = clusters else {
        return (Status::Fail, format!("{} Cluster object(s) (cluster.storm.io), want 1: a node not in a formed cluster", clusters.len()));
    };
    let st = &c["status"];
    let phase = st["phase"].as_str().unwrap_or("?");
    let (m, w) = (st["masters"].as_u64().unwrap_or(0) as usize, st["workers"].as_u64().unwrap_or(0) as usize);
    let name = c["metadata"]["name"].as_str().unwrap_or("?");
    let mut why = Vec::new();
    if phase != "Ready" {
        why.push(format!("phase {phase} ({})", st["message"].as_str().unwrap_or("")));
    }
    if m < masters {
        why.push(format!("{m} master(s), want {masters}"));
    }
    if w < workers {
        why.push(format!("{w} worker(s), want {workers}"));
    }
    if why.is_empty() {
        (Status::Pass, format!("cluster {name} Ready: {m} master(s), {w} worker(s)"))
    } else {
        (Status::Fail, format!("cluster {name}: {}", why.join("; ")))
    }
}

/// `topology/masters` or `topology/workers`: at least `want` members of the
/// role, each `Ready` with its Node Ready.
pub fn judge_role(members: &[Member], role: &str, want: usize) -> (Status, String) {
    let of: Vec<&Member> = members.iter().filter(|m| m.role == role).collect();
    let bad: Vec<String> = of
        .iter()
        .filter(|m| m.phase != "Ready" || !m.node_ready)
        .map(|m| match &m.node {
            None => format!("{}: phase {}, no Node", m.name, m.phase),
            Some(n) => format!("{}: phase {}, Node {n} {}", m.name, m.phase, if m.node_ready { "Ready" } else { "not Ready" }),
        })
        .collect();
    let names: Vec<&str> = of.iter().map(|m| m.name.as_str()).collect();
    if of.len() < want {
        (Status::Fail, format!("{} {role}(s) [{}], want {want}{}", of.len(), names.join(" "), if bad.is_empty() { String::new() } else { format!("; {}", bad.join("; ")) }))
    } else if !bad.is_empty() {
        (Status::Fail, format!("{} of {} {role}(s) not Ready: {}", bad.len(), of.len(), bad.join("; ")))
    } else {
        (Status::Pass, format!("{} {role}(s) Ready: {}", of.len(), names.join(" ")))
    }
}

/// `cni/cross-node`: every serve pod on another worker answered the agent.
pub fn judge_cross(from: &str, probes: &[Probe], targets: &[(String, String)]) -> (Status, String) {
    let mut bad = Vec::new();
    let mut good = Vec::new();
    for (pod, node) in targets {
        match probes.iter().find(|p| &p.to == pod) {
            Some(p) if p.result == "open" => good.push(format!("{node} ({})", p.detail.replace('·', " "))),
            Some(p) => bad.push(format!("{node}: {} {}", p.result, p.detail.replace('·', " "))),
            None => bad.push(format!("{node}: not probed")),
        }
    }
    if targets.is_empty() {
        (Status::Fail, "no serve pod on another worker to reach".into())
    } else if bad.is_empty() {
        (Status::Pass, format!("from {from}: reached {}", good.join(", ")))
    } else {
        (Status::Fail, format!("from {from}: {}", bad.join("; ")))
    }
}

/// The claim helper's `{"claim": …}` line in a pod log, if it has written one.
pub fn claim_state(log: &str) -> Option<(String, String)> {
    log.lines().filter_map(|l| serde_json::from_str::<Value>(&l.replace('\u{b7}', " ")).ok()).find_map(|v| {
        let s = v["claim"].as_str()?.to_string();
        Some((s, v["detail"].as_str().unwrap_or("").to_string()))
    })
}

struct Run<'a> {
    kube: &'a Client,
    ns: String,
    run: String,
    id: String,
    image: String,
    timeout: Duration,
    poll: Duration,
}

impl Run<'_> {
    fn pods(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.ns)
    }

    /// A `/test <args>` pod pinned by `host`, a Node's hostname label.
    fn pod(&self, name: &str, host: &str, args: &[&str], env: Value) -> Value {
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": name, "labels": {"storm.io/test-run": self.run}},
               "spec": {"restartPolicy": "Never", "terminationGracePeriodSeconds": 1, "automountServiceAccountToken": false,
                        "nodeSelector": {"kubernetes.io/hostname": host},
                        "containers": [{"name": "c", "image": self.image, "command": ["/test"], "args": args, "env": env}]}})
    }

    async fn create(&self, path: &str, body: &Value) -> Result<()> {
        let r = self.kube.post(path, body).await?;
        if !r.ok() {
            return Err(anyhow!("creating {} answered {}: {}", body["metadata"]["name"], r.code, r.body));
        }
        Ok(())
    }

    /// Wait until the pod is Running (or has ended) or the timeout: its last state.
    async fn wait_pod(&self, name: &str, done: impl Fn(&Value) -> bool) -> Result<Value> {
        let path = format!("{}/{name}", self.pods());
        let deadline = Instant::now() + self.timeout;
        loop {
            let r = self.kube.get(&path).await?;
            if done(&r.body) || Instant::now() >= deadline {
                return Ok(r.body);
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// DELETE the pod and wait until the API answers 404 for it. Ok only
    /// once it is seen gone: a refused delete, an API error, or a pod still
    /// there at the timeout is an error (finding bac367ba89d8).
    async fn remove_pod(&self, name: &str) -> Result<()> {
        let path = format!("{}/{name}", self.pods());
        let d = self.kube.delete(&path).await.with_context(|| format!("deleting pod {name}"))?;
        if !d.ok() && d.code != 404 {
            return Err(anyhow!("deleting pod {name} answered {}: {}", d.code, d.body));
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            let g = self.kube.get(&path).await.with_context(|| format!("reading pod {name} after its delete"))?;
            if g.code == 404 {
                return Ok(());
            }
            if !g.ok() {
                return Err(anyhow!("reading pod {name} after its delete answered {}: {}", g.code, g.body));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("pod {name} still there {} s after its delete: phase {}", self.timeout.as_secs(), g.body["status"]["phase"]));
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    async fn log(&self, name: &str) -> String {
        self.kube.text(&format!("{}/{name}/log", self.pods())).await.map(|(_, t)| t).unwrap_or_default()
    }
}

fn running(p: &Value) -> bool {
    p["status"]["phase"] == "Running" && p["status"]["podIP"].as_str().is_some_and(|s| !s.is_empty())
}

fn ended(p: &Value) -> bool {
    matches!(p["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed"))
}

pub async fn main(a: Args, full: bool) -> i32 {
    let suite = if full { "full-six" } else { "three-master" };
    let out = Out::new(&a.results, suite);
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
    let workers = if full { a.nodes.saturating_sub(a.masters).max(3) } else { 0 };

    // The cluster, its members and the Nodes (requires.toml cluster_read).
    let listed = async {
        let get = |p: &'static str| {
            let kube = &kube;
            async move {
                match kube.get(p).await {
                    Ok(r) if r.ok() => Ok(kube::items(&r.body)),
                    Ok(r) => Err(format!("{p} answered {} (requires.toml cluster_read)", r.code)),
                    Err(e) => Err(format!("{p}: {e:#}")),
                }
            }
        };
        Ok::<_, String>((
            get("/apis/cluster.storm.io/v1alpha1/clusters").await?,
            get("/apis/cluster.storm.io/v1alpha1/clustermembers").await?,
            get("/api/v1/nodes").await?,
        ))
    }
    .await;
    let (clusters, cms, nodes) = match listed {
        Ok(x) => x,
        Err(why) => {
            out.could_not_run("topology/cluster", t.elapsed(), why);
            out.summary();
            return out.exit_code();
        }
    };
    let (st, d) = judge_cluster(&clusters, a.masters, workers);
    out.emit(Line::new("topology/cluster", st, t.elapsed(), d));
    let ms = members(&cms, &nodes);
    let (st, d) = judge_role(&ms, "master", a.masters);
    out.emit(Line::new("topology/masters", st, t.elapsed(), d));
    if full {
        let (st, d) = judge_role(&ms, "worker", workers);
        out.emit(Line::new("topology/workers", st, t.elapsed(), d));
    }

    // Each master's own apiserver, and one store behind all of them.
    let masters: Vec<&Member> = ms.iter().filter(|m| m.role == "master" && !m.address.is_empty()).collect();
    let ca = std::fs::read(format!("{SA}/ca.crt"));
    let token = a.token_file.clone().unwrap_or_else(|| format!("{SA}/token"));
    let mut direct: Vec<(String, Client)> = Vec::new();
    for m in &masters {
        let t = Instant::now();
        let test = format!("apiserver/{}", m.name);
        let client = match &ca {
            Ok(ca) => Client::with_ca(&format!("https://{}:6443", m.address), &token, ca),
            Err(e) => Err(anyhow!("{SA}/ca.crt: {e}")),
        };
        match client {
            Err(e) => out.could_not_run(test, t.elapsed(), format!("{e:#}")),
            Ok(c) => match c.text("/readyz").await {
                Ok((200, body)) if body.trim() == "ok" => {
                    out.emit(Line::new(test, Status::Pass, t.elapsed(), format!("https://{}:6443/readyz ok", m.address)));
                    direct.push((m.name.clone(), c));
                }
                Ok((code, body)) => out.emit(Line::new(test, Status::Fail, t.elapsed(), format!("https://{}:6443/readyz answered {code}: {}", m.address, body.trim()))),
                Err(e) => out.failure(test, t.elapsed(), &e, format!("https://{}:6443/readyz: {e:#}", m.address)),
            },
        }
    }
    let t = Instant::now();
    match replicated(&direct, &ns, &a.run_id).await {
        Ok(None) => out.emit(Line::new("etcd/replicated", Status::Skip, t.elapsed(), format!("{} master apiserver(s) answered; at least 2 are needed", direct.len()))),
        Ok(Some(d)) => out.emit(Line::new("etcd/replicated", Status::Pass, t.elapsed(), d)),
        Err(e) => out.failure("etcd/replicated", t.elapsed(), &e, format!("{e:#}")),
    }
    out.emit(Line::new("quorum/lose-one", Status::Skip, Duration::ZERO, format!("stopping a master is the runner's, not a Job's ({FAULTS})")));

    if full {
        let image = match a.image.clone() {
            Some(i) => Some(i),
            None => kube::own_image(&kube, &ns, &a.run_id).await,
        };
        let (worker_nodes, unlabelled) = placement(&ms);
        match image {
            None => {
                for test in ["schedule/workers", "cni/cross-node", "csi/reattach"] {
                    out.could_not_run(test, Duration::ZERO, "this test's own image is unknown (not a runner pod and no --image)");
                }
            }
            Some(image) => {
                let id: String = a.run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
                let r = Run { kube: &kube, ns: ns.clone(), run: a.run_id.clone(), id, image, timeout: Duration::from_secs(a.pod_timeout), poll: Duration::from_secs(2) };
                workloads(&r, &worker_nodes, &unlabelled, a.storage_class.as_deref(), &out).await;
            }
        }
        out.emit(Line::new("drain/reboot", Status::Skip, Duration::ZERO, format!("cordon, drain and reboot are node writes, not granted to a Job ({FAULTS})")));
    }
    out.summary();
    out.exit_code()
}

/// `etcd/replicated`: a ConfigMap created through the first master's
/// apiserver is read back, same value and resourceVersion, through every
/// other master's. None when fewer than two masters answered.
async fn replicated(direct: &[(String, Client)], ns: &str, run: &str) -> Result<Option<String>> {
    let [(first, writer), rest @ ..] = direct else { return Ok(None) };
    if rest.is_empty() {
        return Ok(None);
    }
    let name = format!("etcd-{}", run.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>());
    let path = format!("/api/v1/namespaces/{ns}/configmaps");
    let value = format!("{run}-{}", std::process::id());
    let body = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "labels": {"storm.io/test-run": run}}, "data": {"mark": value}});
    let r = writer.post(&path, &body).await?;
    if !r.ok() {
        return Err(anyhow!("creating ConfigMap {name} through {first} answered {}: {}", r.code, r.body));
    }
    let rv = r.body["metadata"]["resourceVersion"].as_str().unwrap_or("").to_string();
    let mut bad = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    for (m, c) in rest {
        loop {
            let got = c.get(&format!("{path}/{name}")).await?;
            let ok = got.ok() && got.body["data"]["mark"].as_str() == Some(value.as_str());
            if ok || Instant::now() >= deadline {
                if !ok {
                    bad.push(format!("{m} answered {} ({})", got.code, got.body["data"]["mark"]));
                }
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    let _ = writer.delete(&format!("{path}/{name}")).await;
    if bad.is_empty() {
        let names: Vec<&str> = rest.iter().map(|(m, _)| m.as_str()).collect();
        Ok(Some(format!("written through {first} (rv {rv}), read back through {}", names.join(" "))))
    } else {
        Err(anyhow!("written through {first} (rv {rv}), not read back: {}", bad.join("; ")))
    }
}

/// `schedule/workers`, `cni/cross-node` and `csi/reattach`. `unlabelled`
/// are Ready workers whose Node has no hostname label: named in
/// `schedule/workers`, never placed on.
async fn workloads(r: &Run<'_>, workers: &[Worker], unlabelled: &[String], class: Option<&str>, out: &Out) {
    let t = Instant::now();
    let no_label: Vec<String> = unlabelled.iter().map(|n| format!("{n}: Node has no kubernetes.io/hostname label, so no nodeSelector can pin a pod to it")).collect();
    if workers.len() < 2 {
        let names: Vec<&str> = workers.iter().map(|w| w.node.as_str()).collect();
        let why = format!("{} placeable Ready worker(s) [{}]: 2 or more are needed", workers.len(), names.join(" "));
        let why = if no_label.is_empty() { why } else { format!("{why}; {}", no_label.join("; ")) };
        for test in ["schedule/workers", "cni/cross-node", "csi/reattach"] {
            out.emit(Line::new(test, Status::Fail, t.elapsed(), why.clone()));
        }
        return;
    }
    // A serve pod on each worker.
    let serve: Vec<(String, &Worker)> = workers.iter().enumerate().map(|(i, w)| (format!("tp-serve-{}-{i}", r.id), w)).collect();
    let mut placed = Vec::new();
    let mut bad = no_label;
    for (pod, w) in &serve {
        if let Err(e) = r.create(&r.pods(), &r.pod(pod, &w.host, &["serve"], json!([]))).await {
            bad.push(format!("{}: {e:#}", w.node));
        }
    }
    for (pod, w) in &serve {
        let node = &w.node;
        match r.wait_pod(pod, |p| running(p) || ended(p)).await {
            Ok(p) if running(&p) && p["spec"]["nodeName"].as_str() == Some(node.as_str()) => {
                placed.push((pod.clone(), node.clone(), p["status"]["podIP"].as_str().unwrap_or("").to_string()))
            }
            Ok(p) => bad.push(format!("{node}: {pod} {} on {}", p["status"]["phase"].as_str().unwrap_or("not created"), p["spec"]["nodeName"].as_str().unwrap_or("no node"))),
            Err(e) => bad.push(format!("{node}: {e:#}")),
        }
    }
    if bad.is_empty() {
        let at: Vec<String> = placed.iter().map(|(_, n, ip)| format!("{n}={ip}")).collect();
        out.emit(Line::new("schedule/workers", Status::Pass, t.elapsed(), format!("a pod on each of {} worker(s): {}", placed.len(), at.join(" "))));
    } else {
        out.emit(Line::new("schedule/workers", Status::Fail, t.elapsed(), bad.join("; ")));
    }

    // From the first worker's agent to the serve pods on the others.
    let t = Instant::now();
    let first = &workers[0];
    let agent = format!("tp-agent-{}", r.id);
    let others: Vec<&(String, String, String)> = placed.iter().filter(|(_, n, _)| n != &first.node).collect();
    let plan = Plan {
        name: agent.clone(),
        user: String::new(),
        timeout_secs: 5,
        vms: vec![],
        targets: others.iter().map(|(p, _, ip)| Target { name: p.clone(), ip: ip.clone(), port: Some(SERVE_PORT), ping: false }).collect(),
    };
    let targets: Vec<(String, String)> = others.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
    let probed = async {
        let env = json!([{"name": "PLAN", "value": serde_json::to_string(&plan)?}]);
        r.create(&r.pods(), &r.pod(&agent, &first.host, &["agent"], env)).await?;
        let p = r.wait_pod(&agent, ended).await?;
        let log = r.log(&agent).await;
        if !ended(&p) {
            return Err(anyhow!("agent on {} did not finish: {}", first.node, p["status"]));
        }
        crate::medium::parse_agent_log(&agent, &log)
    }
    .await;
    match probed {
        Ok(ps) => {
            let (st, d) = judge_cross(&first.node, &ps, &targets);
            out.emit(Line::new("cni/cross-node", st, t.elapsed(), d));
        }
        Err(e) => out.failure("cni/cross-node", t.elapsed(), &e, format!("{e:#}")),
    }

    // A claim written on one worker, read on another.
    let t = Instant::now();
    let (a, b) = (&workers[workers.len() - 1], &workers[if workers.len() > 2 { 1 } else { 0 }]);
    let claim = format!("tp-claim-{}", r.id);
    let claims = format!("/api/v1/namespaces/{}/persistentvolumeclaims", r.ns);
    let moved = reattach(r, &claim, &claims, class, a, b).await;
    // Both claim pods and the claim go, whatever happened; what would not
    // is said (a pod left holds the claim).
    let mut cleanup = Vec::new();
    for step in ["w", "r"] {
        if let Err(e) = r.remove_pod(&format!("{claim}-{step}")).await {
            cleanup.push(format!("{e:#}"));
        }
    }
    match r.kube.delete(&format!("{claims}/{claim}")).await {
        Ok(d) if d.ok() || d.code == 404 => {}
        Ok(d) => cleanup.push(format!("deleting claim {claim} answered {}: {}", d.code, d.body)),
        Err(e) => cleanup.push(format!("deleting claim {claim}: {e:#}")),
    }
    let left = if cleanup.is_empty() { String::new() } else { format!("; cleanup: {}", cleanup.join("; ")) };
    match moved {
        Ok(d) if cleanup.is_empty() => out.emit(Line::new("csi/reattach", Status::Pass, t.elapsed(), format!("claim {claim}: {d}"))),
        Ok(d) => out.emit(Line::new("csi/reattach", Status::Fail, t.elapsed(), format!("claim {claim}: {d}{left}"))),
        Err(e) => out.failure("csi/reattach", t.elapsed(), &e, format!("claim {claim}: {e:#}{left}")),
    }
    for (pod, _) in &serve {
        let _ = r.kube.delete(&format!("{}/{pod}", r.pods())).await;
    }
    let _ = r.kube.delete(&format!("{}/{agent}", r.pods())).await;
}

/// `csi/reattach`'s handoff: the claim is written by a pod on `a`, that pod
/// is seen gone (404), then a pod on `b` finds it, and is seen gone too.
/// A pod that is not seen gone fails the check: the next one must not
/// attach while it may still hold the claim (finding bac367ba89d8).
async fn reattach(r: &Run<'_>, claim: &str, claims: &str, class: Option<&str>, a: &Worker, b: &Worker) -> Result<String> {
    let token = format!("{}-{}", r.run, r.id);
    let claim_pod = |name: &str, host: &str| {
        let mut p = r.pod(name, host, &["claim"], json!([{"name": "CLAIM_TOKEN", "value": token}]));
        p["spec"]["containers"][0]["volumeMounts"] = json!([{"name": "data", "mountPath": "/data"}]);
        p["spec"]["volumes"] = json!([{"name": "data", "persistentVolumeClaim": {"claimName": claim}}]);
        p
    };
    let mut pvc = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": claim, "labels": {"storm.io/test-run": r.run}},
        "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "64Mi"}}}});
    if let Some(c) = class {
        pvc["spec"]["storageClassName"] = json!(c);
    }
    r.create(claims, &pvc).await?;
    let mut seen = Vec::new();
    for (step, w, want) in [("w", a, "written"), ("r", b, "found")] {
        let name = format!("{claim}-{step}");
        r.create(&r.pods(), &claim_pod(&name, &w.host)).await?;
        let deadline = Instant::now() + r.timeout;
        let state = loop {
            if let Some(s) = claim_state(&r.log(&name).await) {
                break Some(s);
            }
            if Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(r.poll).await;
        };
        let p = r.kube.get(&format!("{}/{name}", r.pods())).await?.body;
        let on = p["spec"]["nodeName"].as_str().unwrap_or("no node").to_string();
        let verdict = match state {
            Some((s, _)) if s == want && on == w.node => Ok(format!("{s} on {on}")),
            Some((s, d)) => Err(anyhow!("on {on} (wanted {}) the claim was {s}: {d}", w.node)),
            None => Err(anyhow!("{name} on {on} said nothing about its claim in {} s: phase {}", r.timeout.as_secs(), p["status"]["phase"])),
        };
        // The next pod attaches the claim only once this one let go of it.
        match (verdict, r.remove_pod(&name).await) {
            (Ok(s), Ok(())) => seen.push(s),
            (Err(e), Ok(())) => return Err(e),
            (Ok(s), Err(e)) => return Err(e.context(format!("{s}, but {name} was not seen gone, so the claim was not handed on"))),
            (Err(e), Err(c)) => return Err(anyhow!("{e:#}; and {name} was not seen gone: {c:#}")),
        }
    }
    Ok(seen.join(", then "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, ip: &str, ready: bool) -> Value {
        json!({"metadata": {"name": name}, "status": {"addresses": [{"type": "InternalIP", "address": ip}],
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }

    fn cm(name: &str, role: &str, addr: &str, phase: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"role": role}, "status": {"role": role, "address": addr, "phase": phase}})
    }

    #[test]
    fn members_match_nodes_by_name_or_address() {
        let nodes = [node("m1", "10.0.0.1", true), node("storm-abc", "10.0.0.2", false)];
        let ms = members(&[cm("m1", "master", "", "Ready"), cm("m2", "master", "10.0.0.2", "Ready"), cm("m3", "master", "10.0.0.3", "Joining")], &nodes);
        assert_eq!((ms[0].node.as_deref(), ms[0].address.as_str(), ms[0].node_ready), (Some("m1"), "10.0.0.1", true));
        assert_eq!((ms[1].node.as_deref(), ms[1].node_ready), (Some("storm-abc"), false));
        assert_eq!(ms[2].node, None);
        let (st, d) = judge_role(&ms, "master", 3);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("m2: phase Ready, Node storm-abc not Ready") && d.contains("m3: phase Joining, no Node"), "{d}");
        let ok = members(&[cm("m1", "master", "", "Ready")], &nodes);
        assert_eq!(judge_role(&ok, "master", 1).0, Status::Pass);
        assert!(judge_role(&ok, "master", 3).1.starts_with("1 master(s) [m1], want 3"));
    }

    /// Finding 366cb39723c7: a worker is pinned by its Node's hostname
    /// label, checked by its Node name, and a Node without the label is
    /// named, not pinned on.
    #[test]
    fn workers_are_pinned_by_the_hostname_label_not_the_node_name() {
        let labelled = |name: &str, host: &str, ip: &str| {
            let mut n = node(name, ip, true);
            n["metadata"]["labels"] = json!({"kubernetes.io/hostname": host});
            n
        };
        let nodes = [labelled("n1", "h1", "10.0.1.1"), labelled("n2", "h2", "10.0.1.2"), node("n3", "10.0.1.3", true)];
        let ms = members(&[cm("w1", "worker", "10.0.1.1", "Ready"), cm("w2", "worker", "10.0.1.2", "Ready"), cm("w3", "worker", "10.0.1.3", "Ready")], &nodes);
        assert_eq!((ms[0].node.as_deref(), ms[0].hostname.as_deref()), (Some("n1"), Some("h1")));
        let (ws, unlabelled) = placement(&ms);
        assert_eq!(ws, [Worker { node: "n1".into(), host: "h1".into() }, Worker { node: "n2".into(), host: "h2".into() }]);
        assert_eq!(unlabelled, ["n3"]);
    }

    /// A stand-in apiserver: `answer(method, path)` gives each reply; every
    /// request is recorded as "METHOD path".
    async fn stand_in(answer: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = format!("http://{}", l.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (answer, log) = (std::sync::Arc::new(answer), seen.clone());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let (answer, log) = (answer.clone(), log.clone());
                tokio::spawn(async move {
                    // The whole request (a POST's body too) before the answer.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let n = s.read(&mut chunk).await.unwrap_or(0);
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let Some(end) = text.find("\r\n\r\n") else {
                            if n == 0 {
                                break;
                            }
                            continue;
                        };
                        let len = text[..end].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if n == 0 || buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let mut w = req.split_whitespace();
                    let (m, path) = (w.next().unwrap_or("").to_string(), w.next().unwrap_or("").split('?').next().unwrap_or("").to_string());
                    log.lock().unwrap().push(format!("{m} {path}"));
                    let (code, body) = (answer.as_ref())(&m, &path);
                    let _ = s.write_all(format!("HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        (at, seen)
    }

    fn run(kube: &Client) -> Run<'_> {
        Run { kube, ns: "ns".into(), run: "r1".into(), id: "r1".into(), image: "img".into(), timeout: Duration::from_millis(300), poll: Duration::from_millis(50) }
    }

    /// Finding bac367ba89d8: a pod is removed only once it is seen gone.
    #[tokio::test]
    async fn a_pod_is_removed_only_once_it_answers_404() {
        let gets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let g = gets.clone();
        let (at, _) = stand_in(move |m, _| match m {
            "DELETE" => (200, "{}".into()),
            _ if g.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 => (200, r#"{"status": {"phase": "Running"}}"#.into()),
            _ => (404, "{}".into()),
        })
        .await;
        let kube = Client::new(&at, Some("/nonexistent-token"), false).await.unwrap();
        run(&kube).remove_pod("p").await.unwrap();

        let (at, _) = stand_in(|_, _| (404, "{}".into())).await;
        let kube = Client::new(&at, Some("/nonexistent-token"), false).await.unwrap();
        run(&kube).remove_pod("p").await.expect("already gone is gone");

        let (at, _) = stand_in(|m, _| if m == "DELETE" { (200, "{}".into()) } else { (200, r#"{"status": {"phase": "Running"}}"#.into()) }).await;
        let kube = Client::new(&at, Some("/nonexistent-token"), false).await.unwrap();
        let e = run(&kube).remove_pod("p").await.unwrap_err();
        assert!(format!("{e:#}").contains("still there"), "{e:#}");

        let (at, _) = stand_in(|m, _| if m == "DELETE" { (403, r#"{"reason": "Forbidden"}"#.into()) } else { (404, "{}".into()) }).await;
        let kube = Client::new(&at, Some("/nonexistent-token"), false).await.unwrap();
        let e = run(&kube).remove_pod("p").await.unwrap_err();
        assert!(format!("{e:#}").contains("deleting pod p answered 403"), "{e:#}");
    }

    /// Finding bac367ba89d8: a writer that never goes away fails the
    /// handoff, and no reader is started on the other worker.
    #[tokio::test]
    async fn reattach_fails_when_the_writer_never_goes_away() {
        let (at, seen) = stand_in(|m, path| match (m, path) {
            ("POST", _) => (201, "{}".into()),
            ("GET", p) if p.ends_with("/log") => (200, r#"{"claim": "written", "detail": ""}"#.into()),
            ("GET", _) => (200, r#"{"spec": {"nodeName": "n2"}, "status": {"phase": "Running"}}"#.into()),
            _ => (200, "{}".into()),
        })
        .await;
        let kube = Client::new(&at, Some("/nonexistent-token"), false).await.unwrap();
        let (a, b) = (Worker { node: "n2".into(), host: "h2".into() }, Worker { node: "n1".into(), host: "h1".into() });
        let e = reattach(&run(&kube), "c", "/api/v1/namespaces/ns/persistentvolumeclaims", None, &a, &b).await.unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("written on n2, but c-w was not seen gone") && e.contains("still there"), "{e}");
        let posts: Vec<String> = seen.lock().unwrap().iter().filter(|r| r.starts_with("POST")).cloned().collect();
        assert_eq!(posts, ["POST /api/v1/namespaces/ns/persistentvolumeclaims", "POST /api/v1/namespaces/ns/pods"], "no reader pod");
    }

    #[test]
    fn the_cluster_must_be_one_ready_with_its_members() {
        let c = |phase: &str, m: u64, w: u64| json!({"metadata": {"name": "c1"}, "status": {"phase": phase, "masters": m, "workers": w}});
        assert_eq!(judge_cluster(&[c("Ready", 3, 3)], 3, 3).0, Status::Pass);
        assert_eq!(judge_cluster(&[c("Ready", 3, 0)], 3, 0).0, Status::Pass);
        let (st, d) = judge_cluster(&[c("Forming", 1, 0)], 3, 3);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("phase Forming") && d.contains("1 master(s), want 3") && d.contains("0 worker(s), want 3"), "{d}");
        assert!(judge_cluster(&[], 3, 0).1.starts_with("0 Cluster object(s)"));
    }

    #[test]
    fn cross_node_names_each_worker() {
        let p = |to: &str, result: &str| Probe { from: "a".into(), to: to.into(), proto: "tcp".into(), result: result.into(), detail: "10.1.0.5:8080".into(), attempts: 1 };
        let targets = vec![("s1".to_string(), "w2".to_string()), ("s2".to_string(), "w3".to_string())];
        assert_eq!(judge_cross("w1", &[p("s1", "open"), p("s2", "open")], &targets).0, Status::Pass);
        let (st, d) = judge_cross("w1", &[p("s1", "open"), p("s2", "timeout")], &targets);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("w3: timeout"), "{d}");
        assert!(judge_cross("w1", &[p("s1", "open")], &targets).1.contains("w3: not probed"));
        assert_eq!(judge_cross("w1", &[], &[]).0, Status::Fail);
    }

    #[test]
    fn claim_state_reads_the_helper_line_spaced_or_not() {
        assert_eq!(claim_state("warning: x\n{\"claim\":\"written\",\"detail\":\"\"}\n").map(|s| s.0), Some("written".into()));
        let unspaced = crate::report::unspaced(&json!({"claim": "found", "detail": "token and 1 MiB blob intact"}).to_string());
        assert_eq!(claim_state(&unspaced), Some(("found".into(), "token and 1 MiB blob intact".into())));
        assert_eq!(claim_state("nothing yet"), None);
    }
}
