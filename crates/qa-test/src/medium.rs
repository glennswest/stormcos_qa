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
//! **Up means logged in** (#91): a VMI is `Running` before its guest has
//! booted, so a VM is up once ssh from this driver accepts the run's key.
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
//! Inbound from the node (#23, #92): a host-network agent in the run namespace
//! probes every member by TCP in both passes (`ingress/node-><member>`). The
//! node **must still reach** them under the policy: NetworkPolicy does not
//! apply to traffic from a pod's own node (the kubelet's probes come from
//! it, and the CNIs let host traffic past by default), so a node blocked by
//! the policy is the failure, not a node that gets through.
//! Inbound from the LAN has no vantage point here (nothing in the cluster
//! stands on a LAN host): `ingress/lan->*` is a skip line in every run.
//!
//! **Only what this run made is removed** (#96). Before anything is
//! created, an isolated namespace that already exists must carry this run's
//! `storm.io/test-run` label (the runner's), and none of the names the
//! scenario uses (`agent-key`, `storm-isolate`, `vm-N`, `srv-N`, the agent
//! and outside pods) may exist yet: otherwise `isolation/preflight` could
//! not run, and nothing is touched. Every create the apiserver accepted is
//! recorded with its UID (one whose answer was lost is recovered when the
//! object by that name carries this invocation's `storm.io/qa-instance`
//! label), and cleanup deletes exactly those, each with a UID
//! precondition. A refused create (a namesake made since, 409) is never
//! cleaned up, and neither is a policy this run did not create.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Mutex;
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
    /// The node: an outside target. An IPv4 address, or a name (resolved by
    /// DNS, else the Node's InternalIP); empty: the cluster's only Node (#24).
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
    /// This invocation's label value, `storm.io/qa-instance` (#96): a lost
    /// create answer is recovered only by it, never by a namesake.
    instance: String,
    /// What this run made, by object path and UID: all cleanup touches.
    owned: Mutex<Ledger>,
    key: ssh::Key,
    me: Me,
    started: Instant,
    /// The node's IPv4 address, or why there is none (its egress lines skip).
    node_ip: Result<Ipv4Addr, String>,
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "isolation");
    let started = Instant::now();
    // The ordinary pod network first, in the run namespace (#35): it needs
    // none of what the isolation scenario needs, so it reports even when
    // that cannot run.
    pod_network(&a, &out).await;
    let mut run = match setup(a, out, started).await {
        Ok(r) => r,
        Err((out, Infra(why))) => {
            out.could_not_run("isolation/preflight", started.elapsed(), why);
            out.summary();
            return out.exit_code();
        }
    };
    let res = scenario(&mut run).await;
    if let Err(e) = &res {
        run.out.failure("isolation", run.started.elapsed(), e, format!("{e:#}"));
    }
    cleanup(&run).await;
    run.out.summary();
    run.out.exit_code()
}

/// The pod-network case (#35).
async fn pod_network(a: &Args, out: &Out) {
    let t = Instant::now();
    let fail = |why: String| out.could_not_run("pod-network", t.elapsed(), why);
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
    crate::podnet::run(&kube, &ns, &a.run_id, &image, out).await;
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

    // The isolated namespace: the runner's (labelled with this run), or ours
    // to make. One that is neither is someone else's: refused (#96).
    let instance = instance();
    let mut owned = Ledger::default();
    match kube.get(&format!("/api/v1/namespaces/{iso}")).await {
        Ok(r) if r.ok() => {
            if let Err(why) = namespace_is_the_runs(&r.body, &a.run_id) {
                infra!("{why}");
            }
        }
        Ok(r) if r.code == 404 => {
            let body = json!({"apiVersion": "v1", "kind": "Namespace",
                "metadata": {"name": iso, "labels": {"storm.io/test-run": a.run_id, "storm.io/purpose": "test", INSTANCE: instance}}});
            match kube.post("/api/v1/namespaces", &body).await {
                Ok(r) if r.ok() => owned.made(&format!("/api/v1/namespaces/{iso}"), &r.body),
                Ok(r) => infra!(
                    "namespace {iso} does not exist and creating it answered {}: the runner must provide it (test/requires.toml, stormcentral#55)",
                    r.code
                ),
                Err(e) => infra!("apiserver: {e:#}"),
            }
        }
        Ok(r) => infra!("reading namespace {iso} answered {}: {}", r.code, r.body),
        Err(e) => infra!("apiserver: {e:#}"),
    }
    // From here a namespace made above is removed again on a refusal.
    macro_rules! refuse {
        ($($t:tt)*) => {{
            for (path, uid) in owned.deletions() {
                let _ = kube.delete_uid(&path, &uid).await;
            }
            infra!($($t)*)
        }};
    }
    match kube.get(&kube::vms(&iso)).await {
        Ok(r) if r.ok() => {}
        Ok(r) if r.code == 404 => refuse!("the VirtualMachine resource is not served (kubevirt.io/v1 CRD missing)"),
        Ok(r) => refuse!("listing VirtualMachines in {iso} answered {}: no rights there (stormcentral#55)", r.code),
        Err(e) => refuse!("apiserver: {e:#}"),
    }
    // Nothing the scenario creates may exist yet: a namesake would be
    // refused at its create, or worse, taken for ours (#96).
    let mut taken = Vec::new();
    for path in scenario_objects(&a, &ns, &iso) {
        match kube.get(&path).await {
            Ok(r) if r.code == 404 => {}
            Ok(r) if r.ok() => taken.push(path),
            Ok(r) => refuse!("reading {path} answered {}: {}", r.code, r.body),
            Err(e) => refuse!("apiserver: {e:#}"),
        }
    }
    if !taken.is_empty() {
        // A namespace made just now is ours and empty of these: never here.
        refuse!(
            "already exist, and this run did not create them, so it neither uses nor deletes them: {}",
            taken.join(", ")
        );
    }
    let key = match ssh::Key::generate() {
        Ok(k) => k,
        Err(e) => refuse!("{e:#}"),
    };
    let node_ip = node_ip(&kube, &a.node).await;
    Ok(Run { a, kube, out, ns, iso, instance, owned: Mutex::new(owned), key, me: Me { image }, started, node_ip })
}

const INSTANCE: &str = "storm.io/qa-instance";

fn instance() -> String {
    let mut b = [0u8; 6];
    let _ = getrandom::getrandom(&mut b);
    format!("iso{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// An isolated namespace that exists must be this run's: the runner labels
/// it `storm.io/test-run=<run id>` (stormcentral#183).
fn namespace_is_the_runs(ns: &Value, run_id: &str) -> Result<(), String> {
    let name = ns["metadata"]["name"].as_str().unwrap_or("?");
    match ns["metadata"]["labels"]["storm.io/test-run"].as_str() {
        Some(r) if r == run_id => Ok(()),
        Some(r) => Err(format!("namespace {name} exists and belongs to test run {r}, not {run_id}: not used, not deleted")),
        None => Err(format!(
            "namespace {name} exists without a storm.io/test-run label, so it is not this run's: not used, not deleted (name a free one with --iso-namespace)"
        )),
    }
}

/// Every object the scenario may create, by its path.
fn scenario_objects(a: &Args, ns: &str, iso: &str) -> Vec<String> {
    let mut v = vec![
        format!("/api/v1/namespaces/{iso}/secrets/agent-key"),
        format!("/apis/networking.k8s.io/v1/namespaces/{iso}/networkpolicies/storm-isolate"),
    ];
    v.extend((1..=a.vms).map(|i| format!("{}/vm-{i}", kube::vms(iso))));
    v.extend((1..=a.pods).map(|i| format!("{}/srv-{i}", pods_path(iso))));
    v.extend(["agent-control", "agent-isolated"].map(|p| format!("{}/{p}", pods_path(iso))));
    v.push(format!("{}/outside-{}", pods_path(ns), short_id(&a.run_id)));
    v.extend(["node-probe-control", "node-probe-isolated"].map(|p| format!("{}/{p}", pods_path(ns))));
    v
}

/// What this run made (#96): an object the apiserver accepted, by path and
/// UID, in creation order; and the paths whose create may have landed with
/// its answer lost, resolved at cleanup by this invocation's label.
#[derive(Default, Debug)]
struct Ledger {
    made: Vec<(String, String)>,
    unsure: Vec<String>,
}

impl Ledger {
    fn made(&mut self, path: &str, obj: &Value) {
        match obj["metadata"]["uid"].as_str().filter(|u| !u.is_empty()) {
            Some(uid) => self.made.push((path.to_string(), uid.to_string())),
            None => self.unsure.push(path.to_string()),
        }
    }

    /// An unsure create's object, as read back: ours only when it carries
    /// every label we sent.
    fn recovered(&mut self, path: &str, got: &Value, ours: &Value) {
        let theirs = &got["metadata"]["labels"];
        let all = ours.as_object().is_some_and(|o| !o.is_empty() && o.iter().all(|(k, v)| &theirs[k] == v));
        if all && let Some(uid) = got["metadata"]["uid"].as_str() {
            self.made.push((path.to_string(), uid.to_string()));
        }
    }

    /// Newest first: the namespace, made first, goes last.
    fn deletions(&self) -> Vec<(String, String)> {
        self.made.iter().rev().cloned().collect()
    }
}

/// `STORM_NODE` as an IPv4 address: as given, else by DNS, else the
/// InternalIP of the Node so named; empty, the cluster's only Node's (#24).
async fn node_ip(kube: &Client, node: &str) -> Result<Ipv4Addr, String> {
    if let Ok(ip) = node.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let mut dns = String::new();
    if !node.is_empty() {
        match tokio::net::lookup_host((node, 0)).await {
            Ok(addrs) => {
                let v4 = addrs.into_iter().find_map(|a| match a.ip() {
                    std::net::IpAddr::V4(ip) => Some(ip),
                    std::net::IpAddr::V6(_) => None,
                });
                if let Some(ip) = v4 {
                    return Ok(ip);
                }
                dns = format!("{node} has no IPv4 address in DNS");
            }
            Err(e) => dns = format!("{node} does not resolve ({e})"),
        }
    }
    let nodes = match kube.get("/api/v1/nodes").await {
        Ok(r) if r.ok() => kube::items(&r.body),
        Ok(r) => return Err(format!("{}reading Nodes answered {} (requires.toml cluster_read nodes); pass --node <ip>", lead(&dns), r.code)),
        Err(e) => return Err(format!("{}reading Nodes: {e:#}; pass --node <ip>", lead(&dns))),
    };
    node_from_nodes(&nodes, node).map_err(|e| format!("{}{e}", lead(&dns)))
}

fn lead(dns: &str) -> String {
    if dns.is_empty() { String::new() } else { format!("{dns}; ") }
}

/// The node's InternalIP from the Node list: the one named `want` (by name
/// or any address), or with `want` empty the only Node.
fn node_from_nodes(nodes: &[Value], want: &str) -> Result<Ipv4Addr, String> {
    let named = |n: &&Value| {
        n["metadata"]["name"].as_str() == Some(want) || n["status"]["addresses"].as_array().into_iter().flatten().any(|a| a["address"].as_str() == Some(want))
    };
    let node = match (want.is_empty(), nodes) {
        (true, [n]) => n,
        (true, _) => return Err(format!("STORM_NODE unset and the cluster lists {} Nodes; pass --node <ip>", nodes.len())),
        (false, _) => nodes.iter().find(named).ok_or_else(|| format!("no Node is named {want}; pass --node <ip>"))?,
    };
    let name = node["metadata"]["name"].as_str().unwrap_or("?");
    node["status"]["addresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == "InternalIP")
        .find_map(|a| a["address"].as_str()?.parse::<Ipv4Addr>().ok())
        .ok_or_else(|| format!("Node {name} has no IPv4 InternalIP; pass --node <ip>"))
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
    json!({"storm.io/test-run": run.a.run_id, "app.kubernetes.io/managed-by": "stormcos_qa-isolation", INSTANCE: run.instance})
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

/// On the host's network: no container port (it would claim the host's).
fn host_network(pod: &mut Value) {
    pod["spec"]["hostNetwork"] = json!(true);
    if let Some(c) = pod["spec"]["containers"][0].as_object_mut() {
        c.remove("ports");
    }
}

fn pods_path(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/pods")
}

/// Create, and record what was made (#96). A refusal records nothing: a
/// 409 is a namesake that is not ours. A create whose answer was lost may
/// have landed: cleanup reads it back and takes it only with our labels.
/// Returns the object's path and UID.
async fn create(run: &Run, path: &str, body: &Value) -> Result<(String, String)> {
    let obj = format!("{path}/{}", body["metadata"]["name"].as_str().unwrap_or_default());
    let r = match run.kube.post(path, body).await {
        Ok(r) => r,
        Err(e) => {
            run.owned.lock().unwrap().unsure.push(obj);
            return Err(e);
        }
    };
    anyhow::ensure!(r.ok(), "POST {path} ({}) answered {}: {}", body["metadata"]["name"], r.code, r.body);
    run.owned.lock().unwrap().made(&obj, &r.body);
    Ok((obj, r.body["metadata"]["uid"].as_str().unwrap_or_default().to_string()))
}

/// Delete an object this run made, only if it is still the one it made.
async fn remove(run: &Run, path: &str, uid: &str) {
    if !uid.is_empty() {
        let _ = run.kube.delete_uid(path, uid).await;
    }
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
                run.out.failure(format!("up/{s}"), run.started.elapsed(), &e, format!("{e:#}"));
            }
        }
    }
    // Running is not booted: the control probes the guest's sshd at once, so
    // wait for a login from the driver first (#91). The driver is in another
    // namespace and no policy exists yet, so a VM it cannot log in to is a
    // guest or VM-network fault, said here, not an isolation verdict. The
    // logins are waited for together: one after another, a VM that never
    // takes one spends the shared deadline and the rest are "never tried"
    // (61f60fe76c).
    let mut booting = Vec::new();
    for vm in &vms {
        match wait_vmi(run, vm, deadline).await {
            Ok(ip) => {
                let (addr, user, key) = (format!("{ip}:22"), run.a.ssh_user.clone(), run.key.private.clone());
                let login = tokio::spawn(async move {
                    let r = ssh::wait_login(&addr, &user, key, deadline).await;
                    (r, Instant::now())
                });
                booting.push((vm, ip, login));
            }
            Err(e) => {
                down += 1;
                let ev = evidence(run, &kube::vms(&iso), &kube::vmis(&iso), vm).await;
                run.out.failure(format!("up/{vm}"), run.started.elapsed(), &e, format!("{e:#}; {ev}"));
            }
        }
    }
    for (vm, ip, login) in booting {
        let (r, at) = login.await.unwrap_or_else(|e| (Err(anyhow!("the login wait ended: {e}")), Instant::now()));
        let took = at.duration_since(run.started);
        match r {
            Ok(()) => {
                run.out.emit(Line::new(format!("up/{vm}"), Status::Pass, took, format!("Running at {ip}, ssh from the driver accepted the run's key")));
                members.push(Member { name: vm.clone(), ip, port: 22, vm: true });
            }
            Err(e) => {
                down += 1;
                let ev = evidence(run, &kube::vms(&iso), &kube::vmis(&iso), vm).await;
                run.out.failure(
                    format!("up/{vm}"),
                    took,
                    &e,
                    format!("Running at {ip}, but no login from the driver (another namespace, no policy yet): {e:#}; the guest or the VM network, not isolation; {ev}"),
                );
            }
        }
    }
    let outside_ip = match wait_pod(run, &run.ns, &outside_name, deadline).await {
        Ok(ip) => ip,
        Err(e) => {
            run.out.failure("up/outside", run.started.elapsed(), &e, format!("{e:#}"));
            return Ok(());
        }
    };
    if down > 0 {
        return Ok(()); // each is already a failure line with its evidence
    }

    let (targets, unprobed) = targets(run, &members, &outside_ip);
    let plan = |name: &str| Plan {
        name: name.into(),
        user: run.a.ssh_user.clone(),
        timeout_secs: run.a.probe_timeout,
        vms: members.iter().filter(|m| m.vm).map(|m| (m.name.clone(), m.ip.clone())).collect(),
        targets: targets.clone(),
    };

    // Control: without the policy.
    let t0 = Instant::now();
    let control = run_agent(run, &run.iso, &plan("agent-control"), false).await?;
    let control_in = inbound(run, &members).await;
    let control_node = node_in(run, &members, "control").await;
    save(run, "control", &control, &control_in, &control_node).await;
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
    let isolated = run_agent(run, &run.iso, &plan("agent-isolated"), false).await?;
    let isolated_in = inbound(run, &members).await;
    let isolated_node = node_in(run, &members, "isolated").await;
    save(run, "isolated", &isolated, &isolated_in, &isolated_node).await;
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
    // Outside targets that could not be formed: said, never left out (#24).
    for (to, why) in &unprobed {
        run.out.emit(Line::new(format!("egress/*->{to}"), Status::Skip, Duration::ZERO, format!("not probed: {why}")));
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
    // the node (host network) -> inside: must still answer (#23, #92).
    for (name, st, detail) in judge_node(&members, &control_node, &isolated_node) {
        run.out.emit(Line::new(format!("ingress/node->{name}"), st, Duration::ZERO, detail));
    }
    run.out.emit(Line::new(
        "ingress/lan->*",
        Status::Skip,
        Duration::ZERO,
        "not probed: no vantage point on a LAN host (the test runs inside the cluster; #23)",
    ));
    Ok(())
}

/// The node's view: a host-network agent in the run namespace probes every
/// member by TCP. Err: it could not be run (said as a skip, never dropped).
async fn node_in(run: &Run, members: &[Member], pass: &str) -> Result<Vec<Probe>, String> {
    let plan = Plan {
        name: format!("node-probe-{pass}"),
        user: run.a.ssh_user.clone(),
        timeout_secs: run.a.probe_timeout,
        vms: Vec::new(),
        targets: members.iter().map(|m| Target { name: m.name.clone(), ip: m.ip.clone(), port: Some(m.port), ping: false }).collect(),
    };
    match run_agent(run, &run.ns, &plan, true).await {
        Ok(ps) => Ok(ps.into_iter().map(|p| Probe { from: "node".into(), ..p }).collect()),
        Err(e) => Err(format!("the host-network probe pod ({pass} pass) could not run: {e:#}")),
    }
}

/// `ingress/node-><member>` verdicts (#92): still answering the node under
/// the policy where it answered without it. The node's own traffic is not
/// subject to NetworkPolicy (kubelet probes depend on it), so reaching the
/// member is the pass and a block is the failure.
fn judge_node(members: &[Member], control: &Result<Vec<Probe>, String>, isolated: &Result<Vec<Probe>, String>) -> Vec<(String, Status, String)> {
    let (c, i) = match (control, isolated) {
        (Err(e), _) | (_, Err(e)) => return vec![("*".into(), Status::Skip, format!("not probed: {e}"))],
        (Ok(c), Ok(i)) => (c, i),
    };
    let find = |ps: &[Probe], to: &str| ps.iter().find(|p| p.to == to).cloned();
    members
        .iter()
        .map(|m| {
            let (st, detail) = match (find(c, &m.name), find(i, &m.name)) {
                (None, _) | (_, None) => (Status::Skip, format!("the node probe reported nothing for {}", m.name)),
                (Some(c), _) if !c.reached() => (Status::Skip, format!("{} did not answer the node even without the policy (tcp {})", m.name, c.result)),
                (_, Some(p)) if p.reached() => (Status::Pass, format!("the node still reaches it under the policy (host traffic is not policed): tcp {}", p.result)),
                (_, Some(p)) => (Status::Fail, format!("the policy blocks the node's own traffic (kubelet probes would fail): tcp {}", p.result)),
            };
            (m.name.clone(), st, detail)
        })
        .collect()
}

/// The agent's name differs between passes; everything else is by name.
fn same_from(a: &str, b: &str) -> bool {
    a == b || (a.starts_with("agent-") && b.starts_with("agent-"))
}

fn short_id(run_id: &str) -> String {
    run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect()
}

/// Every target, and the outside targets that could not be formed, with why.
fn targets(run: &Run, members: &[Member], outside_ip: &str) -> (Vec<Target>, Vec<(String, String)>) {
    let mut t: Vec<Target> = members.iter().map(|m| Target { name: m.name.clone(), ip: m.ip.clone(), port: Some(m.port), ping: true }).collect();
    t.push(Target { name: "other-namespace-pod".into(), ip: outside_ip.into(), port: Some(SERVE_PORT), ping: true });
    let (outside, unprobed) = outside_targets(&run.node_ip, run.a.lan_target.as_deref(), &run.a.internet_target);
    t.extend(outside);
    (t, unprobed)
}

/// The node (`:6443`), the LAN (`--lan-target`, else `<node /24>.1`) and the
/// internet; a target without an address is returned as (name, why).
fn outside_targets(node: &Result<Ipv4Addr, String>, lan: Option<&str>, internet: &str) -> (Vec<Target>, Vec<(String, String)>) {
    let (mut t, mut unprobed) = (Vec::new(), Vec::new());
    match node {
        Ok(ip) => t.push(Target { name: "node".into(), ip: ip.to_string(), port: Some(6443), ping: true }),
        Err(why) => unprobed.push(("node".into(), why.clone())),
    }
    let lan = match (lan, node) {
        (Some(l), _) => Ok(l.to_string()),
        (None, Ok(ip)) => {
            let o = ip.octets();
            Ok(format!("{}.{}.{}.1", o[0], o[1], o[2]))
        }
        (None, Err(why)) => Err(format!("no --lan-target and no node address for <node /24>.1: {why}")),
    };
    match lan {
        Ok(l) => {
            let (ip, port) = split_target(&l);
            t.push(Target { name: "lan".into(), ip, port, ping: true });
        }
        Err(why) => unprobed.push(("lan".into(), why)),
    }
    let (ip, port) = split_target(internet);
    t.push(Target { name: "internet".into(), ip, port, ping: true });
    (t, unprobed)
}

fn split_target(s: &str) -> (String, Option<u16>) {
    match s.rsplit_once(':') {
        Some((ip, p)) if p.parse::<u16>().is_ok() => (ip.to_string(), p.parse().ok()),
        _ => (s.to_string(), None),
    }
}

/// The driver's own TCP probes into every member — from another namespace,
/// concurrently, each retried while nothing answers (#57).
async fn inbound(run: &Run, members: &[Member]) -> Vec<Probe> {
    let t = Duration::from_secs(run.a.probe_timeout);
    let mut set = tokio::task::JoinSet::new();
    for (i, m) in members.iter().cloned().enumerate() {
        set.spawn(async move {
            let (result, attempts) = retry::probe(&retry::Policy::PROBE, &format!("tcp {}:{}", m.ip, m.port), |_| async {
                let r = match tokio::time::timeout(t, tokio::net::TcpStream::connect((m.ip.as_str(), m.port))).await {
                    Err(_) => "timeout",
                    Ok(Ok(_)) => "open",
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => "refused",
                    Ok(Err(_)) => "error",
                };
                (r, r == "open" || r == "refused")
            })
            .await;
            (i, Probe { from: "other-namespace-driver".into(), to: m.name, proto: "tcp".into(), result: result.into(), detail: String::new(), attempts })
        });
    }
    let mut out: Vec<(usize, Probe)> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(p) = r {
            out.push(p);
        }
    }
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, p)| p).collect()
}

/// Start an agent pod in the isolated namespace, wait for it, read its log.
async fn run_agent(run: &Run, ns: &str, plan: &Plan, host: bool) -> Result<Vec<Probe>> {
    let mut env = vec![json!({"name": "PLAN", "value": serde_json::to_string(plan)?})];
    if !plan.vms.is_empty() {
        env.push(json!({"name": "SSH_KEY", "valueFrom": {"secretKeyRef": {"name": "agent-key", "key": "SSH_KEY"}}}));
    }
    let mut body = pod(run, ns, &plan.name, &["agent"], Value::Array(env));
    if host {
        host_network(&mut body);
    }
    let (path, uid) = create(run, &pods_path(ns), &body).await?;
    let deadline = Instant::now() + Duration::from_secs(run.a.probe_timeout * 8 + 300);
    loop {
        let r = run.kube.get(&path).await?;
        match r.body["status"]["phase"].as_str() {
            Some("Succeeded") | Some("Failed") => break,
            _ if Instant::now() >= deadline => {
                remove(run, &path, &uid).await;
                return Err(anyhow!("agent {} did not finish in time: {}", plan.name, r.body["status"]));
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    let log = run.kube.get(&format!("{path}/log")).await?;
    let text = match &log.body {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let _ = tokio::fs::write(run.a.results.join(format!("{}.log", plan.name)), &text).await;
    remove(run, &path, &uid).await;
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

async fn save(run: &Run, pass: &str, a: &[Probe], b: &[Probe], node: &Result<Vec<Probe>, String>) {
    let node = match node {
        Ok(ps) => json!(ps),
        Err(e) => json!({"error": e}),
    };
    let v = json!({"agent": a, "from_another_namespace": b, "from_the_node": node});
    let _ = tokio::fs::write(run.a.results.join(format!("isolation-{pass}.json")), serde_json::to_vec_pretty(&v).unwrap_or_default()).await;
}

async fn wait_vmi(run: &Run, vm: &str, deadline: Instant) -> Result<String> {
    let path = format!("{}/{vm}", kube::vmis(&run.iso));
    let mut last = String::from("no VMI yet");
    let mut failed_look = None;
    // Look at least once: an earlier wait may have used the shared deadline up,
    // and "no VMI yet" without a look would be a false failure.
    loop {
        if let Some(r) = kube::looked(run.kube.get(&path).await, &mut failed_look)
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
    Err(kube::timed_out(failed_look, format!("not Running with a pod-network address in time: {last} (stormvm#16)")))
}

async fn wait_pod(run: &Run, ns: &str, name: &str, deadline: Instant) -> Result<String> {
    let path = format!("{}/{name}", pods_path(ns));
    let mut last = String::from("no pod yet");
    let mut failed_look = None;
    // Look at least once, as in wait_vmi.
    loop {
        if let Some(r) = kube::looked(run.kube.get(&path).await, &mut failed_look)
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
    Err(kube::timed_out(failed_look, format!("{ns}/{name} not Running with an IP in time: {last}")))
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

/// Exactly what this run made, each by its UID (#96): a namesake that was
/// there before, or made since, is left alone. The runner deletes the
/// namespaces too; this leaves the host clean when run by hand.
async fn cleanup(run: &Run) {
    let unsure = std::mem::take(&mut run.owned.lock().unwrap().unsure);
    let ours = json!({INSTANCE: run.instance});
    for path in unsure {
        if let Ok(r) = run.kube.get(&path).await
            && r.ok()
        {
            run.owned.lock().unwrap().recovered(&path, &r.body, &ours);
        }
    }
    let gone = run.owned.lock().unwrap().deletions();
    for (path, uid) in gone {
        remove(run, &path, &uid).await;
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
    fn the_node_comes_from_the_node_list() {
        let n = |name: &str, ip: &str| json!({"metadata": {"name": name}, "status": {"addresses": [{"type": "InternalIP", "address": ip}, {"type": "Hostname", "address": name}]}});
        let one = [n("storm-06f96d", "192.168.30.2")];
        assert_eq!(node_from_nodes(&one, ""), Ok("192.168.30.2".parse().unwrap()));
        assert_eq!(node_from_nodes(&one, "storm-06f96d"), Ok("192.168.30.2".parse().unwrap()));
        let two = [n("a", "10.0.0.1"), n("b", "10.0.0.2")];
        assert_eq!(node_from_nodes(&two, "b"), Ok("10.0.0.2".parse().unwrap()));
        assert!(node_from_nodes(&two, "").unwrap_err().contains("lists 2 Nodes"));
        assert!(node_from_nodes(&two, "c").unwrap_err().contains("no Node is named c"));
        let v6 = [json!({"metadata": {"name": "x"}, "status": {"addresses": [{"type": "InternalIP", "address": "fd00::1"}]}})];
        assert!(node_from_nodes(&v6, "").unwrap_err().contains("no IPv4 InternalIP"));
    }

    #[test]
    fn a_target_without_an_address_is_said_not_dropped() {
        let names = |t: &[Target]| t.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        let (t, un) = outside_targets(&Ok("192.168.30.2".parse().unwrap()), None, "1.1.1.1:443");
        assert_eq!(names(&t), ["node", "lan", "internet"]);
        assert_eq!(t[1].ip, "192.168.30.1");
        assert!(un.is_empty());
        let (t, un) = outside_targets(&Err("STORM_NODE unset".into()), None, "1.1.1.1:443");
        assert_eq!(names(&t), ["internet"]);
        assert_eq!(un.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["node", "lan"]);
        assert!(un[1].1.contains("STORM_NODE unset"));
        let (t, un) = outside_targets(&Err("x".into()), Some("10.0.0.1:22"), "1.1.1.1:443");
        assert_eq!(names(&t), ["lan", "internet"]);
        assert_eq!((t[0].ip.as_str(), t[0].port), ("10.0.0.1", Some(22)));
        assert_eq!(un.len(), 1);
    }

    #[test]
    fn the_node_still_reaches_members_under_the_policy() {
        let m = |n: &str| Member { name: n.into(), ip: "10.0.0.1".into(), port: 8080, vm: false };
        let members = [m("a"), m("b"), m("c")];
        let p = |to: &str, r: &str| Probe { from: "node".into(), to: to.into(), proto: "tcp".into(), result: r.into(), detail: String::new(), attempts: 1 };
        let control = Ok(vec![p("a", "open"), p("b", "open"), p("c", "timeout")]);
        let isolated = Ok(vec![p("a", "timeout"), p("b", "open"), p("c", "timeout")]);
        let v: Vec<(String, Status)> = judge_node(&members, &control, &isolated).into_iter().map(|(n, s, _)| (n, s)).collect();
        assert_eq!(v, [("a".into(), Status::Fail), ("b".into(), Status::Pass), ("c".into(), Status::Skip)]);
        let v = judge_node(&members, &control, &Err("403".into()));
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].0.as_str(), v[0].1), ("*", Status::Skip));
        assert!(v[0].2.contains("403"));
    }

    #[test]
    fn the_node_probe_pod_is_on_the_host_network() {
        let mut p = json!({"spec": {"containers": [{"name": "c", "ports": [{"containerPort": 8080}]}]}});
        host_network(&mut p);
        assert_eq!(p["spec"]["hostNetwork"], true);
        assert!(p["spec"]["containers"][0].get("ports").is_none());
    }

    #[test]
    fn an_agent_that_did_not_finish_is_an_error() {
        let ok = "{\"from\":\"a\",\"to\":\"b\",\"proto\":\"tcp\",\"result\":\"open\"}\nnoise\n{\"agent\":\"done\"}\n";
        assert_eq!(parse_agent_log("a", ok).unwrap().len(), 1);
        assert!(parse_agent_log("a", "{\"from\":\"a\",\"to\":\"b\",\"proto\":\"tcp\",\"result\":\"open\"}\n").is_err());
    }

    // ---- #96: cleanup removes only what this run made -------------------

    /// A store of objects by path: POST makes one (409 on a namesake), DELETE
    /// honours a UID precondition (409 on a mismatch), and every DELETE is kept.
    #[derive(Default)]
    struct Api {
        objs: BTreeMap<String, Value>,
        deletes: Vec<(String, Option<String>)>,
        next: u64,
    }

    const COLLECTIONS: [&str; 5] = ["/virtualmachines", "/pods", "/secrets", "/networkpolicies", "/namespaces"];

    impl Api {
        fn put(&mut self, path: &str, uid: &str, labels: Value) {
            let name = path.rsplit('/').next().unwrap();
            self.objs.insert(path.into(), json!({"metadata": {"name": name, "uid": uid, "labels": labels}}));
        }

        fn handle(&mut self, method: &str, path: &str, body: &[u8]) -> (u16, String) {
            let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            match method {
                "GET" if self.objs.contains_key(path) => (200, self.objs[path].to_string()),
                "GET" if COLLECTIONS.iter().any(|c| path.ends_with(c)) => {
                    let items: Vec<&Value> = self.objs.iter().filter(|(k, _)| k.rsplit_once('/').is_some_and(|(p, _)| p == path)).map(|(_, v)| v).collect();
                    (200, json!({"items": items}).to_string())
                }
                "POST" => {
                    let obj = format!("{path}/{}", body["metadata"]["name"].as_str().unwrap());
                    if self.objs.contains_key(&obj) {
                        return (409, json!({"reason": "AlreadyExists"}).to_string());
                    }
                    self.next += 1;
                    let mut b = body.clone();
                    b["metadata"]["uid"] = json!(format!("ours-{}", self.next));
                    self.objs.insert(obj, b.clone());
                    (201, b.to_string())
                }
                "DELETE" => {
                    let want = body["preconditions"]["uid"].as_str().map(str::to_string);
                    self.deletes.push((path.into(), want.clone()));
                    match self.objs.get(path) {
                        None => (404, "{}".into()),
                        Some(o) if want.as_ref().is_some_and(|w| o["metadata"]["uid"] != w.as_str()) => (409, json!({"reason": "Conflict"}).to_string()),
                        Some(_) => {
                            self.objs.remove(path);
                            (200, "{}".into())
                        }
                    }
                }
                _ => (404, "{}".into()),
            }
        }
    }

    async fn serve(api: std::sync::Arc<Mutex<Api>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let api = api.clone();
                tokio::spawn(async move {
                    let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
                    let head_end = loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
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
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let mut first = head.lines().next().unwrap_or_default().split(' ');
                    let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
                    let path = target.split('?').next().unwrap_or_default();
                    let (code, text) = api.lock().unwrap().handle(&method, path, &buf[head_end..(head_end + len).min(buf.len())]);
                    let resp = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        url
    }

    const ISO: &str = "/api/v1/namespaces/run-ns-iso";

    async fn start(api: &std::sync::Arc<Mutex<Api>>, name: &str) -> Result<Run, String> {
        let url = serve(api.clone()).await;
        let results = std::env::temp_dir().join(format!("qa-medium-{name}-{}", std::process::id()));
        let argv = [
            "medium", "--api", &url, "--token-file", "/nonexistent", "--namespace", "run-ns", "--run-id", "r1", "--node", "10.0.0.9",
            "--image", "img", "--results", &results.display().to_string(), "--vms", "2", "--pods", "1",
        ];
        let a = Args::try_parse_from(argv).unwrap();
        let out = Out::new(&a.results, "isolation");
        setup(a, out, Instant::now()).await.map_err(|(_, Infra(why))| why)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn namesakes_this_run_did_not_create_are_refused_and_left_alone() {
        let api = std::sync::Arc::new(Mutex::new(Api::default()));
        {
            let mut a = api.lock().unwrap();
            a.put(ISO, "ns-uid", json!({"storm.io/test-run": "r1"}));
            a.put(&format!("{ISO}/secrets/agent-key"), "theirs-1", json!({}));
            a.put("/apis/networking.k8s.io/v1/namespaces/run-ns-iso/networkpolicies/storm-isolate", "theirs-2", json!({}));
            a.put("/apis/kubevirt.io/v1/namespaces/run-ns-iso/virtualmachines/vm-1", "theirs-3", json!({}));
        }
        let why = start(&api, "namesakes").await.err().expect("refused");
        for n in ["secrets/agent-key", "networkpolicies/storm-isolate", "virtualmachines/vm-1"] {
            assert!(why.contains(n), "{why}");
        }
        let a = api.lock().unwrap();
        assert!(a.deletes.is_empty(), "{:?}", a.deletes);
        assert_eq!(a.objs.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_isolated_namespace_that_is_not_the_runs_is_refused() {
        for (labels, says) in [(json!({"storm.io/test-run": "other"}), "test run other"), (json!({}), "without a storm.io/test-run label")] {
            let api = std::sync::Arc::new(Mutex::new(Api::default()));
            api.lock().unwrap().put(ISO, "ns-uid", labels);
            let why = start(&api, "notours").await.err().expect("refused");
            assert!(why.contains(says), "{why}");
            assert!(api.lock().unwrap().deletes.is_empty());
        }
    }

    /// The finding's case: a create refused (409) part way. Cleanup deletes
    /// what was made, each by its UID, newest first, the namespace last;
    /// not the namesake, nor one that replaced an object of ours since.
    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_deletes_only_what_this_run_made_by_uid() {
        let api = std::sync::Arc::new(Mutex::new(Api::default()));
        let mut run = start(&api, "partial").await.expect("set up");
        // A namesake appears after the preflight: srv-1's create is refused.
        let srv = format!("{ISO}/pods/srv-1");
        api.lock().unwrap().put(&srv, "theirs-srv", json!({}));
        let e = scenario(&mut run).await.unwrap_err();
        assert!(format!("{e:#}").contains("409"), "{e:#}");
        // And vm-2 is replaced by someone else's since it was made.
        let vm2 = "/apis/kubevirt.io/v1/namespaces/run-ns-iso/virtualmachines/vm-2";
        let vm1 = "/apis/kubevirt.io/v1/namespaces/run-ns-iso/virtualmachines/vm-1";
        let made_vm2 = api.lock().unwrap().objs[vm2]["metadata"]["uid"].as_str().unwrap().to_string();
        api.lock().unwrap().put(vm2, "theirs-vm2", json!({}));
        cleanup(&run).await;
        let a = api.lock().unwrap();
        let paths: Vec<&str> = a.deletes.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, [vm2, vm1, &format!("{ISO}/secrets/agent-key"), ISO]);
        assert!(a.deletes.iter().all(|(_, u)| u.as_deref().is_some_and(|u| u.starts_with("ours-"))), "{:?}", a.deletes);
        assert_eq!(a.deletes[0].1.as_deref(), Some(made_vm2.as_str()));
        assert_eq!(a.objs[vm2]["metadata"]["uid"], "theirs-vm2", "the replacement stays");
        assert_eq!(a.objs[&srv]["metadata"]["uid"], "theirs-srv", "the namesake stays");
        assert!(!paths.iter().any(|p| p.contains("storm-isolate")), "no policy was made, so none is deleted");
    }

    #[test]
    fn a_lost_create_is_ours_only_with_this_invocations_label() {
        let mut l = Ledger::default();
        let ours = json!({INSTANCE: "iso1"});
        l.recovered("/p/a", &json!({"metadata": {"uid": "u1", "labels": {INSTANCE: "iso2"}}}), &ours);
        l.recovered("/p/b", &json!({"metadata": {"uid": "u2", "labels": {}}}), &ours);
        l.recovered("/p/c", &json!({"metadata": {"uid": "u3", "labels": {INSTANCE: "iso1", "x": "y"}}}), &ours);
        assert_eq!(l.deletions(), [("/p/c".to_string(), "u3".to_string())]);
        l.made("/p/d", &json!({"metadata": {}}));
        assert_eq!(l.unsure, ["/p/d"], "made without a UID: read back at cleanup");
    }
}
