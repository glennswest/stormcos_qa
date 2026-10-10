//! `/test medium` — namespace isolation (#18).
//!
//! Owner, 2026-09-24: "I should be able to have 5 intercommunicating VMs in a
//! namespace with no outside traffic." Isolation is exactly what
//! stormconsole's "isolated namespace" action applies — the NetworkPolicy
//! `storm-isolate` (every pod and VM in the namespace reaches every other;
//! nothing in, nothing out), enforced by Cilium, which covers a VM only once
//! it is a real pod-network endpoint (stormvm#16).
//!
//! **Where the driver stands.** A pod inside an isolated namespace cannot
//! reach the apiserver either, so this driver (the runner's Job, in the run
//! namespace) stays **outside** and builds the isolated namespace next to it:
//! `STORM_NAMESPACE_ISO`, else `<run ns>-iso`, labelled with the run so the
//! runner deletes it. In it: N VMs and M server pods. In the run namespace:
//! one server pod, the "another namespace's pod". Probing from inside is done
//! by an **agent** pod (`/test agent`) in the isolated namespace, which logs
//! in to every VM and probes from it, and prints its results to its log —
//! read through the API, which needs no pod network.
//!
//! **Control first.** Everything is probed once before the policy: the
//! members must reach each other (else the pod network is broken, not
//! isolated), and an outside target that is unreachable even then says
//! nothing about the policy — it is reported **skip**, never pass.
//!
//! Then the policy goes on, the driver waits until it is enforced (its own
//! connections to the members stop getting answers), and everything is probed
//! again: inside ↔ inside must answer; inside → another namespace's pod, the
//! node, the LAN and the internet must not; another namespace's pod (this
//! driver) → inside must not.
//!
//! Not covered: inbound from the node or the LAN (no host-network vantage
//! point under the runner's namespace-only Role).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;
use serde_json::{Value, json};

use crate::agent::{Plan, Probe, SERVE_PORT, Target};
use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};
use crate::ssh;

#[derive(Parser, Debug)]
#[command(name = "test medium", about = "Namespace isolation: VMs and pods in an isolated namespace talk to each other and nothing else (#18)")]
pub struct Args {
    #[arg(long, env = "STORM_API", default_value = "")]
    api: String,
    #[arg(long)]
    token_file: Option<String>,
    #[arg(long)]
    insecure: bool,
    /// The run's namespace: the driver and the outside pod live here.
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    /// The namespace to isolate (default <namespace>-iso; created if absent).
    #[arg(long, env = "STORM_NAMESPACE_ISO")]
    iso_namespace: Option<String>,
    #[arg(long, env = "STORM_RUN_ID", default_value = "manual")]
    run_id: String,
    /// The node's address: an outside target.
    #[arg(long, env = "STORM_NODE", default_value = "")]
    node: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: PathBuf,
    /// This test's image, for the helper pods (default: the Job pod's own).
    #[arg(long)]
    image: Option<String>,
    #[arg(long, default_value_t = 5)]
    vms: usize,
    #[arg(long, default_value_t = 2)]
    pods: usize,
    #[arg(long, default_value = "fedora-44-x86_64")]
    golden: String,
    #[arg(long, default_value = "fedora")]
    ssh_user: String,
    #[arg(long, default_value_t = 1024)]
    vm_memory_mib: u64,
    /// Members up (VMI Running with an address, pods Running).
    #[arg(long, default_value_t = 900)]
    ready_timeout: u64,
    /// From creating the policy until the driver's probes stop being answered.
    #[arg(long, default_value_t = 120)]
    enforce_timeout: u64,
    /// Seconds per probe: no answer in this long is `timeout`.
    #[arg(long, default_value_t = 3)]
    probe_timeout: u64,
    /// A LAN host: `ip` (ping only) or `ip:port`. Default: <node's /24>.1.
    #[arg(long)]
    lan_target: Option<String>,
    #[arg(long, default_value = "1.1.1.1:443")]
    internet_target: String,
}

struct Infra(String);

struct Me {
    image: String,
}

struct Run {
    a: Args,
    kube: Client,
    out: Out,
    ns: String,
    iso: String,
    created_iso: bool,
    key: ssh::Key,
    me: Me,
    started: Instant,
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "isolation");
    let started = Instant::now();
    // The ordinary pod network first, in the run namespace (#35): it needs
    // none of what the isolation scenario needs, so it reports even when
    // that cannot run.
    let pn_failed = pod_network(&a, &out).await;
    let mut run = match setup(a, out, started).await {
        Ok(r) => r,
        Err((out, Infra(why))) => {
            out.emit(Line::new("isolation/preflight", Status::Fail, started.elapsed(), why));
            out.summary();
            return if pn_failed > 0 { 1 } else { 2 };
        }
    };
    let res = scenario(&mut run).await;
    if let Err(e) = &res {
        run.out.emit(Line::new("isolation", Status::Fail, run.started.elapsed(), format!("{e:#}")));
    }
    cleanup(&run).await;
    run.out.summary();
    if run.out.failed() > 0 { 1 } else { 0 }
}

/// The pod-network case (#35); returns how many of its lines failed.
async fn pod_network(a: &Args, out: &Out) -> usize {
    let t = Instant::now();
    let fail = |why: String| {
        out.emit(Line::new("pod-network", Status::Fail, t.elapsed(), format!("could not run: {why}")));
        1
    };
    let kube = match Client::new(&a.api, a.token_file.as_deref(), a.insecure).await {
        Ok(k) => k,
        Err(e) => return fail(format!("{e:#}")),
    };
    let ns = match &a.namespace {
        Some(n) => n.clone(),
        None => match tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").await {
            Ok(n) => n.trim().to_string(),
            Err(_) => return fail("no --namespace / STORM_NAMESPACE and no service-account namespace".into()),
        },
    };
    let image = match &a.image {
        Some(i) => i.clone(),
        None => match kube::own_image(&kube, &ns, &a.run_id).await {
            Some(i) => i,
            None => return fail(format!("this test's image not found in {ns}; pass --image")),
        },
    };
    crate::podnet::run(&kube, &ns, &a.run_id, &image, out).await.0
}

// ---- setup -----------------------------------------------------------------

async fn setup(a: Args, out: Out, started: Instant) -> Result<Run, (Out, Infra)> {
    macro_rules! infra {
        ($($t:tt)*) => { return Err((out, Infra(format!($($t)*)))) };
    }
    let kube = match Client::new(&a.api, a.token_file.as_deref(), a.insecure).await {
        Ok(k) => k,
        Err(e) => infra!("{e:#}"),
    };
    let ns = match &a.namespace {
        Some(n) => n.clone(),
        None => match tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").await {
            Ok(n) => n.trim().to_string(),
            Err(_) => infra!("no --namespace / STORM_NAMESPACE and no service-account namespace"),
        },
    };
    let iso = a.iso_namespace.clone().unwrap_or_else(|| iso_name(&ns, &a.run_id));

    let image = match &a.image {
        Some(i) => i.clone(),
        None => {
            match kube::own_image(&kube, &ns, &a.run_id).await {
                Some(i) => i,
                None => infra!("cannot find this test's own pod in {ns} to learn its image; pass --image"),
            }
        }
    };

    // The isolated namespace: the runner's, or ours to make.
    let mut created_iso = false;
    match kube.get(&format!("/api/v1/namespaces/{iso}")).await {
        Ok(r) if r.ok() => {}
        Ok(_) => {
            let body = json!({"apiVersion": "v1", "kind": "Namespace",
                "metadata": {"name": iso, "labels": {"storm.io/test-run": a.run_id, "storm.io/purpose": "test"}}});
            match kube.post("/api/v1/namespaces", &body).await {
                Ok(r) if r.ok() => created_iso = true,
                Ok(r) => infra!(
                    "namespace {iso} does not exist and creating it answered {}: the runner must provide it (test/requires.toml, stormcentral#55)",
                    r.code
                ),
                Err(e) => infra!("apiserver: {e:#}"),
            }
        }
        Err(e) => infra!("apiserver: {e:#}"),
    }
    match kube.get(&kube::vms(&iso)).await {
        Ok(r) if r.ok() => {}
        Ok(r) if r.code == 404 => infra!("the VirtualMachine resource is not served (kubevirt.io/v1 CRD missing)"),
        Ok(r) => infra!("listing VirtualMachines in {iso} answered {}: no rights there (stormcentral#55)", r.code),
        Err(e) => infra!("apiserver: {e:#}"),
    }
    let key = match ssh::Key::generate() {
        Ok(k) => k,
        Err(e) => infra!("{e:#}"),
    };
    Ok(Run { a, kube, out, ns, iso, created_iso, key, me: Me { image }, started })
}

/// `<ns>-iso`, kept a DNS label: when too long, the run id keeps it unique.
pub fn iso_name(ns: &str, run_id: &str) -> String {
    let n = format!("{ns}-iso");
    if n.len() <= 63 {
        return n;
    }
    let id: String = run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
    format!("qa-iso-{id}")
}

/// Exactly the policy stormconsole's "isolated namespace" action creates
/// (stormconsole crates/plugins/kubernetes/src/projects.rs `isolation`,
/// without the opt-in DNS policy).
pub fn storm_isolate(ns: &str) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {"name": "storm-isolate", "namespace": ns, "labels": {"storm.io/isolation": "true"}},
        "spec": {
            "podSelector": {},
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [{"from": [{"podSelector": {}}]}],
            "egress": [{"to": [{"podSelector": {}}]}],
        }
    })
}

fn labels(run: &Run) -> Value {
    json!({"storm.io/test-run": run.a.run_id, "app.kubernetes.io/managed-by": "stormcos_qa-isolation"})
}

fn vm_manifest(run: &Run, name: &str) -> Value {
    // The key goes in the user-data: this test is about the network, not
    // about accessCredentials (stormvm#41, which #16 covers).
    let user_data = format!("#cloud-config\nssh_authorized_keys:\n  - {}\n", run.key.public_openssh);
    json!({
        "apiVersion": "kubevirt.io/v1",
        "kind": "VirtualMachine",
        "metadata": {"name": name, "namespace": run.iso, "labels": labels(run)},
        "spec": {
            "running": true,
            "template": {
                "metadata": {"labels": labels(run)},
                "spec": {
                    "domain": {
                        "cpu": {"cores": 1},
                        "memory": {"guest": format!("{}Mi", run.a.vm_memory_mib)},
                        "resources": {"requests": {"memory": format!("{}Mi", run.a.vm_memory_mib)}},
                        "devices": {
                            "disks": [
                                {"name": "root", "disk": {"bus": "virtio"}},
                                {"name": "seed", "disk": {"bus": "virtio"}}
                            ],
                            // On the pod network, holding the pod IP: the
                            // only path NetworkPolicy reaches (stormvm#16).
                            "interfaces": [{"name": "default", "bridge": {}}]
                        }
                    },
                    "networks": [{"name": "default", "pod": {}}],
                    "volumes": [
                        {"name": "root", "dataVolume": {"name": run.a.golden}},
                        {"name": "seed", "cloudInitNoCloud": {"userData": user_data}}
                    ]
                }
            }
        }
    })
}

fn pod(run: &Run, ns: &str, name: &str, args: &[&str], env: Value) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run)},
        "spec": {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 1,
            "automountServiceAccountToken": false,
            "containers": [{
                "name": "c",
                "image": run.me.image,
                "command": ["/test"],
                "args": args,
                "env": env,
                "ports": [{"containerPort": SERVE_PORT, "protocol": "TCP"}],
            }],
        }
    })
}

fn pods_path(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/pods")
}

async fn create(run: &Run, path: &str, body: &Value) -> Result<()> {
    let r = run.kube.post(path, body).await?;
    anyhow::ensure!(r.ok(), "POST {path} ({}) answered {}: {}", body["metadata"]["name"], r.code, r.body);
    Ok(())
}

// ---- the scenario -----------------------------------------------------------

#[derive(Clone, Debug)]
struct Member {
    name: String,
    ip: String,
    port: u16,
    vm: bool,
}

async fn scenario(run: &mut Run) -> Result<()> {
    let iso = run.iso.clone();
    let vms: Vec<String> = (1..=run.a.vms).map(|i| format!("vm-{i}")).collect();
    let srvs: Vec<String> = (1..=run.a.pods).map(|i| format!("srv-{i}")).collect();

    create(run, &format!("/api/v1/namespaces/{iso}/secrets"), &json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": "agent-key", "namespace": iso, "labels": labels(run)},
        "stringData": {"SSH_KEY": run.key.private_openssh()?}
    }))
    .await?;
    for vm in &vms {
        create(run, &kube::vms(&iso), &vm_manifest(run, vm)).await?;
    }
    for s in &srvs {
        create(run, &pods_path(&iso), &pod(run, &iso, s, &["serve"], json!([]))).await?;
    }
    let outside_name = format!("outside-{}", short_id(&run.a.run_id));
    create(run, &pods_path(&run.ns), &pod(run, &run.ns, &outside_name, &["serve"], json!([]))).await?;

    // Up.
    let deadline = Instant::now() + Duration::from_secs(run.a.ready_timeout);
    let mut members = Vec::new();
    let mut down = 0;
    // Pods first: they come up in seconds, and a VM wait that times out
    // would otherwise leave the pods only one look.
    for s in &srvs {
        match wait_pod(run, &iso, s, deadline).await {
            Ok(ip) => {
                run.out.emit(Line::new(format!("up/{s}"), Status::Pass, run.started.elapsed(), format!("Running at {ip}")));
                members.push(Member { name: s.clone(), ip, port: SERVE_PORT, vm: false });
            }
            Err(e) => {
                down += 1;
                run.out.emit(Line::new(format!("up/{s}"), Status::Fail, run.started.elapsed(), format!("{e:#}")));
            }
        }
    }
    for vm in &vms {
        match wait_vmi(run, vm, deadline).await {
            Ok(ip) => {
                run.out.emit(Line::new(format!("up/{vm}"), Status::Pass, run.started.elapsed(), format!("Running at {ip}")));
                members.push(Member { name: vm.clone(), ip, port: 22, vm: true });
            }
            Err(e) => {
                down += 1;
                let ev = evidence(run, &kube::vms(&iso), &kube::vmis(&iso), vm).await;
                run.out.emit(Line::new(format!("up/{vm}"), Status::Fail, run.started.elapsed(), format!("{e:#}; {ev}")));
            }
        }
    }
    let outside_ip = match wait_pod(run, &run.ns, &outside_name, deadline).await {
        Ok(ip) => ip,
        Err(e) => {
            run.out.emit(Line::new("up/outside", Status::Fail, run.started.elapsed(), format!("{e:#}")));
            return Ok(());
        }
    };
    if down > 0 {
        return Ok(()); // each is already a failure line with its evidence
    }

    let targets = targets(run, &members, &outside_ip);
    let plan = |name: &str| Plan {
        name: name.into(),
        user: run.a.ssh_user.clone(),
        timeout_secs: run.a.probe_timeout,
        vms: members.iter().filter(|m| m.vm).map(|m| (m.name.clone(), m.ip.clone())).collect(),
        targets: targets.clone(),
    };

    // Control: without the policy.
    let t0 = Instant::now();
    let control = run_agent(run, &plan("agent-control")).await?;
    let control_in = inbound(run, &members).await;
    save(run, "control", &control, &control_in).await;
    let member_names: Vec<&str> = members.iter().map(|m| m.name.as_str()).chain(["agent-control"]).collect();
    let mut broken = Vec::new();
    for p in control.iter().filter(|p| member_names.contains(&p.to.as_str()) && !p.reached()) {
        broken.push(format!("{} -> {} ({})", p.from, p.to, p.proto));
    }
    let silent: Vec<String> = members.iter().filter(|m| m.vm && !control.iter().any(|p| p.from == m.name)).map(|m| m.name.clone()).collect();
    if !broken.is_empty() || !silent.is_empty() {
        run.out.emit(Line::new(
            "control",
            Status::Fail,
            t0.elapsed(),
            format!(
                "without any policy the members do not all reach each other, so isolation cannot be judged: {}{}",
                broken.join(", "),
                if silent.is_empty() { String::new() } else { format!("; no probes from {} (ssh from the agent failed)", silent.join(", ")) }
            ),
        ));
        return Ok(());
    }
    run.out.emit(Line::new("control", Status::Pass, t0.elapsed(), format!("{} members reach each other without a policy", members.len())));
    let meaningful = |p: &Probe| control.iter().chain(&control_in).any(|c| c.to == p.to && c.proto == p.proto && same_from(&c.from, &p.from) && c.reached());

    // Isolate, and wait until the driver's own connections go unanswered.
    let t0 = Instant::now();
    create(run, &format!("/apis/networking.k8s.io/v1/namespaces/{iso}/networkpolicies"), &storm_isolate(&iso)).await?;
    let enforce_deadline = Instant::now() + Duration::from_secs(run.a.enforce_timeout);
    let open_before: Vec<&Probe> = control_in.iter().filter(|p| p.reached()).collect();
    loop {
        let now = inbound(run, &members).await;
        let still: Vec<String> = now.iter().filter(|p| p.reached() && open_before.iter().any(|b| b.to == p.to)).map(|p| p.to.clone()).collect();
        if still.is_empty() {
            run.out.emit(Line::new("policy-enforced", Status::Pass, t0.elapsed(), "storm-isolate created; the driver's connections into the namespace go unanswered"));
            break;
        }
        if Instant::now() >= enforce_deadline {
            run.out.emit(Line::new(
                "policy-enforced",
                Status::Fail,
                t0.elapsed(),
                format!("{}s after storm-isolate, still reachable from another namespace: {}", run.a.enforce_timeout, still.join(", ")),
            ));
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Isolated.
    let isolated = run_agent(run, &plan("agent-isolated")).await?;
    let isolated_in = inbound(run, &members).await;
    save(run, "isolated", &isolated, &isolated_in).await;
    let member_names: Vec<&str> = members.iter().map(|m| m.name.as_str()).chain(["agent-isolated"]).collect();

    // inside <-> inside: must answer (all protocols of a pair).
    let mut pairs: BTreeMap<(String, String), Vec<&Probe>> = BTreeMap::new();
    for p in &isolated {
        pairs.entry((p.from.clone(), p.to.clone())).or_default().push(p);
    }
    for ((from, to), ps) in &pairs {
        let desc = ps.iter().map(|p| format!("{} {}", p.proto, p.result)).collect::<Vec<_>>().join(", ");
        if member_names.contains(&to.as_str()) {
            let ok = ps.iter().all(|p| p.reached());
            run.out.emit(Line::new(format!("inside/{from}->{to}"), if ok { Status::Pass } else { Status::Fail }, Duration::ZERO, desc));
        } else {
            // inside -> outside: must not answer, where it did without the policy.
            let judged: Vec<&&Probe> = ps.iter().filter(|p| meaningful(p)).collect();
            let (st, detail) = if judged.is_empty() {
                (Status::Skip, format!("{to} was not reachable from {from} even without the policy ({desc})"))
            } else if judged.iter().any(|p| p.reached()) {
                (Status::Fail, format!("reached through the policy: {desc}"))
            } else {
                (Status::Pass, format!("blocked: {desc}"))
            };
            run.out.emit(Line::new(format!("egress/{from}->{to}"), st, Duration::ZERO, detail));
        }
    }
    for vm in members.iter().filter(|m| m.vm) {
        if !isolated.iter().any(|p| p.from == vm.name) {
            run.out.emit(Line::new(format!("inside/agent-isolated->{}", vm.name), Status::Fail, Duration::ZERO, "the agent could not log in to it under the policy"));
        }
    }
    // another namespace -> inside: must not answer.
    for p in &isolated_in {
        let (st, detail) = if !meaningful(p) {
            (Status::Skip, format!("{} did not answer another namespace even without the policy", p.to))
        } else if p.reached() {
            (Status::Fail, format!("reached from another namespace through the policy: tcp {}", p.result))
        } else {
            (Status::Pass, format!("blocked: tcp {}", p.result))
        };
        run.out.emit(Line::new(format!("ingress/{}->{}", p.from, p.to), st, Duration::ZERO, detail));
    }
    Ok(())
}

/// The agent's name differs between passes; everything else is by name.
fn same_from(a: &str, b: &str) -> bool {
    a == b || (a.starts_with("agent-") && b.starts_with("agent-"))
}

fn short_id(run_id: &str) -> String {
    run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect()
}

fn targets(run: &Run, members: &[Member], outside_ip: &str) -> Vec<Target> {
    let mut t: Vec<Target> = members.iter().map(|m| Target { name: m.name.clone(), ip: m.ip.clone(), port: Some(m.port), ping: true }).collect();
    t.push(Target { name: "other-namespace-pod".into(), ip: outside_ip.into(), port: Some(SERVE_PORT), ping: true });
    if !run.a.node.is_empty() {
        t.push(Target { name: "node".into(), ip: run.a.node.clone(), port: Some(6443), ping: true });
    }
    let lan = run.a.lan_target.clone().or_else(|| {
        let ip: std::net::Ipv4Addr = run.a.node.parse().ok()?;
        let o = ip.octets();
        Some(format!("{}.{}.{}.1", o[0], o[1], o[2]))
    });
    if let Some(l) = lan {
        let (ip, port) = split_target(&l);
        t.push(Target { name: "lan".into(), ip, port, ping: true });
    }
    let (ip, port) = split_target(&run.a.internet_target);
    t.push(Target { name: "internet".into(), ip, port, ping: true });
    t
}

fn split_target(s: &str) -> (String, Option<u16>) {
    match s.rsplit_once(':') {
        Some((ip, p)) if p.parse::<u16>().is_ok() => (ip.to_string(), p.parse().ok()),
        _ => (s.to_string(), None),
    }
}

/// The driver's own TCP probes into every member — from another namespace.
async fn inbound(run: &Run, members: &[Member]) -> Vec<Probe> {
    let t = Duration::from_secs(run.a.probe_timeout);
    let futs = members.iter().map(|m| async move {
        let r = tokio::time::timeout(t, tokio::net::TcpStream::connect((m.ip.as_str(), m.port))).await;
        let result = match r {
            Err(_) => "timeout",
            Ok(Ok(_)) => "open",
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => "refused",
            Ok(Err(_)) => "error",
        };
        Probe { from: "other-namespace-driver".into(), to: m.name.clone(), proto: "tcp".into(), result: result.into(), detail: String::new() }
    });
    let mut out = Vec::new();
    for f in futs {
        out.push(f.await);
    }
    out
}

/// Start an agent pod in the isolated namespace, wait for it, read its log.
async fn run_agent(run: &Run, plan: &Plan) -> Result<Vec<Probe>> {
    let env = json!([
        {"name": "PLAN", "value": serde_json::to_string(plan)?},
        {"name": "SSH_KEY", "valueFrom": {"secretKeyRef": {"name": "agent-key", "key": "SSH_KEY"}}}
    ]);
    create(run, &pods_path(&run.iso), &pod(run, &run.iso, &plan.name, &["agent"], env)).await?;
    let path = format!("{}/{}", pods_path(&run.iso), plan.name);
    let deadline = Instant::now() + Duration::from_secs(run.a.probe_timeout * 8 + 300);
    loop {
        let r = run.kube.get(&path).await?;
        match r.body["status"]["phase"].as_str() {
            Some("Succeeded") | Some("Failed") => break,
            _ if Instant::now() >= deadline => return Err(anyhow!("agent {} did not finish in time: {}", plan.name, r.body["status"])),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    let log = run.kube.get(&format!("{path}/log")).await?;
    let text = match &log.body {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let _ = tokio::fs::write(run.a.results.join(format!("{}.log", plan.name)), &text).await;
    let _ = run.kube.delete(&path).await;
    parse_agent_log(&plan.name, &text)
}

pub fn parse_agent_log(name: &str, text: &str) -> Result<Vec<Probe>> {
    let mut probes = Vec::new();
    let mut done = false;
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if v["agent"] == "done" {
            done = true;
        } else if let Ok(p) = serde_json::from_value::<Probe>(v) {
            probes.push(p);
        }
    }
    anyhow::ensure!(done, "agent {name} did not finish; its log ends: {}", text.lines().rev().take(5).collect::<Vec<_>>().join(" | "));
    Ok(probes)
}

async fn save(run: &Run, pass: &str, a: &[Probe], b: &[Probe]) {
    let v = json!({"agent": a, "from_another_namespace": b});
    let _ = tokio::fs::write(run.a.results.join(format!("isolation-{pass}.json")), serde_json::to_vec_pretty(&v).unwrap_or_default()).await;
}

async fn wait_vmi(run: &Run, vm: &str, deadline: Instant) -> Result<String> {
    let path = format!("{}/{vm}", kube::vmis(&run.iso));
    let mut last = String::from("no VMI yet");
    // Look at least once: an earlier wait may have used the shared deadline up,
    // and "no VMI yet" without a look would be a false failure.
    loop {
        if let Ok(r) = run.kube.get(&path).await
            && r.ok()
        {
            let s = &r.body["status"];
            let ip = s["interfaces"].as_array().into_iter().flatten().filter_map(|i| i["ipAddress"].as_str()).find(|i| !i.is_empty());
            match (s["phase"].as_str(), ip) {
                (Some("Running"), Some(ip)) => return Ok(ip.to_string()),
                (phase, ip) => last = format!("phase {phase:?}, address {ip:?}, message {:?}", s["message"].as_str().unwrap_or("")),
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow!("not Running with a pod-network address in time: {last} (stormvm#16)"))
}

async fn wait_pod(run: &Run, ns: &str, name: &str, deadline: Instant) -> Result<String> {
    let path = format!("{}/{name}", pods_path(ns));
    let mut last = String::from("no pod yet");
    // Look at least once, as in wait_vmi.
    loop {
        if let Ok(r) = run.kube.get(&path).await
            && r.ok()
        {
            let s = &r.body["status"];
            match (s["phase"].as_str(), s["podIP"].as_str()) {
                (Some("Running"), Some(ip)) if !ip.is_empty() => return Ok(ip.to_string()),
                (phase, ip) => last = format!("phase {phase:?}, podIP {ip:?}"),
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow!("{ns}/{name} not Running with an IP in time: {last}"))
}

async fn evidence(run: &Run, vms: &str, vmis: &str, name: &str) -> String {
    let mut ev = json!({});
    for (k, p) in [("vm", format!("{vms}/{name}")), ("vmi", format!("{vmis}/{name}"))] {
        ev[k] = run.kube.get(&p).await.map(|r| r.body).unwrap_or(Value::Null);
    }
    let file = run.a.results.join(format!("isolation-{name}.json"));
    let _ = tokio::fs::write(&file, serde_json::to_vec_pretty(&ev).unwrap_or_default()).await;
    format!("VMI status {}; evidence in {}", ev["vmi"]["status"], file.display())
}

/// Everything the run made. The runner deletes the namespaces too; this
/// leaves the host clean when run by hand.
async fn cleanup(run: &Run) {
    let iso = &run.iso;
    for i in 1..=run.a.vms {
        let _ = run.kube.delete(&format!("{}/vm-{i}", kube::vms(iso))).await;
    }
    for p in (1..=run.a.pods).map(|i| format!("srv-{i}")).chain(["agent-control".into(), "agent-isolated".into()]) {
        let _ = run.kube.delete(&format!("{}/{p}", pods_path(iso))).await;
    }
    let _ = run.kube.delete(&format!("{}/outside-{}", pods_path(&run.ns), short_id(&run.a.run_id))).await;
    let _ = run.kube.delete(&format!("/apis/networking.k8s.io/v1/namespaces/{iso}/networkpolicies/storm-isolate")).await;
    let _ = run.kube.delete(&format!("/api/v1/namespaces/{iso}/secrets/agent-key")).await;
    if run.created_iso {
        let _ = run.kube.delete(&format!("/api/v1/namespaces/{iso}")).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_namespace_stays_a_dns_label() {
        assert_eq!(iso_name("test-stormcos_qa-medium-r1", "r1"), "test-stormcos_qa-medium-r1-iso");
        let long = "x".repeat(62);
        let n = iso_name(&long, "Run-ID_42");
        assert!(n.len() <= 63 && n.starts_with("qa-iso-runid42"), "{n}");
    }

    #[test]
    fn the_policy_is_the_consoles() {
        let p = storm_isolate("ns");
        assert_eq!(p["metadata"]["name"], "storm-isolate");
        assert_eq!(p["spec"]["podSelector"], json!({}));
        assert_eq!(p["spec"]["ingress"], json!([{"from": [{"podSelector": {}}]}]));
        assert_eq!(p["spec"]["egress"], json!([{"to": [{"podSelector": {}}]}]));
    }

    #[test]
    fn targets_split() {
        assert_eq!(split_target("1.1.1.1:443"), ("1.1.1.1".into(), Some(443)));
        assert_eq!(split_target("192.168.1.1"), ("192.168.1.1".into(), None));
    }

    #[test]
    fn an_agent_that_did_not_finish_is_an_error() {
        let ok = "{\"from\":\"a\",\"to\":\"b\",\"proto\":\"tcp\",\"result\":\"open\"}\nnoise\n{\"agent\":\"done\"}\n";
        assert_eq!(parse_agent_log("a", ok).unwrap().len(), 1);
        assert!(parse_agent_log("a", "{\"from\":\"a\",\"to\":\"b\",\"proto\":\"tcp\",\"result\":\"open\"}\n").is_err());
    }
}
