//! `/test short` — what `medium` and `long` stand on, in under 2 minutes:
//! the apiserver answers with this run's credentials, every Node is Ready
//! and every platform pod is up (#34, stormcos#51's smoke test), the VirtualMachine
//! resource is served, the Linux golden the VM suites clone is on the node,
//! and a helper pod started from this test's own image comes up and answers
//! (the same way `medium` starts its servers and agents). `sc-login` proves
//! a node login's `sc` works: tier-0's node-admin token and CA reach the
//! apiserver as `/etc/profile.d/sc.sh` configures them (#59, stormcos#60).
//! `boot-source` records which boot mode the run tested (#5): the node's
//! own disk, a flow-over still under way, or diskless from the network.
//! `ssh-keys` reads every Ready node's ssh report, ConfigMap
//! `kube-system/ssh-keys-<node>` (#66, stormcos#521): fresh, sshd on :22,
//! both key files read, the expected keys present — through the API, no ssh.
//! `engine-audit` reads the engine's `audit.log` (#79, stormcos#368): a
//! destructive call still let through only by audit mode would be refused
//! once the engine enforces (stormcos#250, stormblock#274).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::agent::SERVE_PORT;
use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};

#[derive(Parser, Debug)]
#[command(name = "test short", about = "Prerequisites of the VM suites (< 2 min)")]
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
    #[arg(long, env = "STORM_NODE", default_value = "127.0.0.1")]
    node: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: PathBuf,
    #[arg(long, default_value = "fedora-44-x86_64")]
    golden: String,
    /// stormblock's API (default <node>:9090, https if it offers TLS, #81).
    #[arg(long, env = "STORMBLOCK_URL")]
    stormblock_url: Option<String>,
    /// The CA an https engine is checked against (default the node CA).
    #[arg(long, env = "STORMBLOCK_CA_FILE")]
    stormblock_ca: Option<PathBuf>,
    /// A platform pod restarted more often than this fails `system-pods`,
    /// even when it is Running at the moment of the check (#34).
    #[arg(long, default_value_t = 3)]
    max_restarts: u64,
    /// How long `node-ready` and `system-pods` wait for a clean answer
    /// before they report what they see (the whole suite has 120 s).
    #[arg(long, default_value_t = 30)]
    settle: u64,
    /// Key fingerprints (`SHA256:…`) every node must let in, for
    /// `ssh-keys` (#66): in the baked or the install-config file. Empty:
    /// only that each node has baked keys (the run does not pass its list yet).
    #[arg(long, env = "STORM_SSH_KEYS", value_delimiter = ',')]
    ssh_keys: Vec<String>,
    /// A node's ssh report older than this fails `ssh-keys`: the reporter
    /// rewrites it at least every 30 minutes (stormcos#521).
    #[arg(long, default_value_t = 35 * 60)]
    ssh_keys_max_age: u64,
    /// This test's image (default: the Job pod's own).
    #[arg(long)]
    image: Option<String>,
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "short");
    let t = Instant::now();
    let kube = match Client::new(&a.api, a.token_file.as_deref(), a.insecure).await {
        Ok(k) => k,
        Err(e) => {
            out.emit(Line::new("api", Status::Fail, t.elapsed(), format!("{e:#}")));
            out.summary();
            return 2;
        }
    };
    let ns = match &a.namespace {
        Some(n) => n.clone(),
        None => std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").unwrap_or_else(|_| "default".into()).trim().to_string(),
    };

    // The apiserver, with this run's credentials. Nothing else can run without it.
    let pods = format!("/api/v1/namespaces/{ns}/pods");
    let me = match kube.get(&pods).await {
        Ok(r) if r.ok() => {
            out.emit(Line::new("api", Status::Pass, t.elapsed(), format!("listed pods in {ns}")));
            let host = std::env::var("HOSTNAME").unwrap_or_default();
            kube::items(&r.body).into_iter().find(|p| p["metadata"]["name"].as_str() == Some(host.as_str()))
        }
        other => {
            let why = match other {
                Ok(r) => format!("listing pods in {ns} answered {}: {}", r.code, r.body),
                Err(e) => format!("{e:#}"),
            };
            out.emit(Line::new("api", Status::Fail, t.elapsed(), why));
            out.summary();
            return 2;
        }
    };

    // The smoke test (#34): the node is Ready and the platform's own pods
    // are up. Both read cluster-wide (requires.toml `[short]` cluster_read);
    // a refusal is "could not run", never a pass.
    let t = Instant::now();
    let deadline = t + Duration::from_secs(a.settle);
    let checked = loop {
        let got = async {
            let nodes = list(&kube, "/api/v1/nodes").await?;
            let nss = list(&kube, "/api/v1/namespaces").await?;
            let pods = list(&kube, "/api/v1/pods").await?;
            Ok::<_, String>((node_ready(&nodes), system_pods(&nss, &pods, a.max_restarts)))
        }
        .await;
        match got {
            Ok((n, p)) if (n.0 == Status::Fail || p.0 == Status::Fail) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            other => break other,
        }
    };
    match checked {
        Ok((n, p)) => {
            out.emit(Line::new("node-ready", n.0, t.elapsed(), n.1));
            out.emit(Line::new("system-pods", p.0, t.elapsed(), p.1));
        }
        Err(why) => {
            for test in ["node-ready", "system-pods"] {
                out.could_not_run(test, t.elapsed(), &why);
            }
        }
    }

    // The node itself (#30: the stormpump-era form of the old tests/ scripts).
    let host = std::env::var("STORM_HOST_ROOT").unwrap_or_default();
    let t = Instant::now();
    match list(&kube, "/api/v1/nodes").await {
        Ok(nodes) => {
            let (st, d) = node_identity(&nodes);
            out.emit(Line::new("node-identity", st, t.elapsed(), d));
        }
        Err(why) => {
            out.could_not_run("node-identity", t.elapsed(), why);
        }
    }

    // Every node's ssh keys and sshd (#66), from its own report.
    let t = Instant::now();
    let got = async {
        let nodes = list(&kube, "/api/v1/nodes").await?;
        let cms = list(&kube, &format!("/api/v1/namespaces/kube-system/configmaps?labelSelector={SSH_KEYS_LABEL}%3Dtrue")).await?;
        Ok::<_, String>(ssh_keys(&nodes, &cms, &a.ssh_keys, unix_now(), a.ssh_keys_max_age))
    }
    .await;
    match got {
        Ok((st, d)) => out.emit(Line::new("ssh-keys", st, t.elapsed(), d)),
        Err(why) => out.could_not_run("ssh-keys", t.elapsed(), why),
    }
    for (test, file, check) in [
        ("node-stack", "/run/stormpump/assets.json", node_stack as fn(&str, u64) -> (Status, String)),
        ("root", "/proc/1/mountinfo", |m: &str, _| root_mount(m)),
    ] {
        let t = Instant::now();
        let path = format!("{host}{file}");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (st, d) = check(&text, a.max_restarts);
                out.emit(Line::new(test, st, t.elapsed(), d));
            }
            Err(e) => {
                out.could_not_run(test, t.elapsed(), format!("{path}: {e} (requires.toml [short] host_paths_read_only)"));
            }
        }
    }
    let t = Instant::now();
    let (st, d) = sc_login(&host, &a.node).await;
    if st == Status::Skip {
        out.could_not_run("sc-login", t.elapsed(), d);
    } else {
        out.emit(Line::new("sc-login", st, t.elapsed(), d));
    }

    let t = Instant::now();
    let path = format!("{host}{ENGINE_AUDIT_LOG}");
    let (st, d) = match std::fs::read(&path) {
        Ok(bytes) => {
            let (st, d, records) = engine_audit(&String::from_utf8_lossy(&bytes));
            for r in records {
                eprintln!("{}", crate::report::unspaced(&format!("engine-audit: {r}")));
            }
            (st, d)
        }
        Err(e) => (Status::Skip, format!("{path}: {e}: an absent log proves nothing either way (requires.toml [short] host_paths_read_only)")),
    };
    if st == Status::Skip {
        out.could_not_run("engine-audit", t.elapsed(), d);
    } else {
        out.emit(Line::new("engine-audit", st, t.elapsed(), d));
    }

    let t = Instant::now();
    let (st, d) = ssh_banner(&a.node).await;
    out.emit(Line::new("ssh", st, t.elapsed(), d));

    let t = Instant::now();
    match kube.get(&kube::vms(&ns)).await {
        Ok(r) if r.ok() => out.emit(Line::new("vm-resource", Status::Pass, t.elapsed(), "kubevirt.io/v1 virtualmachines served")),
        Ok(r) => out.emit(Line::new("vm-resource", Status::Fail, t.elapsed(), format!("listing VirtualMachines answered {}: {}", r.code, r.body))),
        Err(e) => out.failure("vm-resource", t.elapsed(), &e, format!("{e:#}")),
    }

    let t = Instant::now();
    let token = crate::census::stormblock_token();
    match crate::engine::resolve(a.stormblock_url.as_deref(), &a.node, a.stormblock_ca.as_deref(), token.as_deref(), Duration::from_secs(20)).await {
        Ok(engine) => engine_checks(&out, &a.golden, &engine, token.is_some()).await,
        Err(e) => {
            for check in ["golden", "boot-source"] {
                out.failure(check, t.elapsed(), &e, format!("stormblock client: {e:#}"));
            }
        }
    }

    // A helper pod from this image answers.
    let t = Instant::now();
    let image = a.image.clone().or_else(|| me.as_ref().and_then(|p| p["spec"]["containers"][0]["image"].as_str().map(str::to_string)));
    match image {
        None => out.emit(Line::new("helper-pod", Status::Skip, t.elapsed(), "not running as a pod and no --image")),
        Some(image) => {
            let name = format!("serve-{}", a.run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>());
            let (st, d) = helper(&kube, &pods, &name, &image, &a.run_id).await;
            let _ = kube.delete(&format!("{pods}/{name}")).await;
            out.emit(Line::new("helper-pod", st, t.elapsed(), d));
        }
    }
    out.summary();
    out.exit_code()
}

/// The checks that read the engine: the golden is on it (#42), and which
/// boot mode this run tests (#5, its open health).
async fn engine_checks(out: &Out, golden: &str, engine: &crate::engine::Engine, token: bool) {
    let t = Instant::now();
    match crate::census::volume_named(&engine.http, &engine.base, golden).await {
        Ok(true) => out.emit(Line::new("golden", Status::Pass, t.elapsed(), format!("{golden} is on the node"))),
        Ok(false) => out.emit(Line::new("golden", Status::Fail, t.elapsed(), format!("{golden} is not among the node's stormblock volumes"))),
        Err(e) => out.failure(
            "golden",
            t.elapsed(),
            &e,
            format!(
                "stormblock refused the volume list ({e:#}; {})",
                if token { "token sent" } else { "no token found: STORMBLOCK_API_TOKEN, STORMBLOCK_TOKEN_FILE, /etc/stormblock/api_token or <host>/run/stormblock/engine/api_token" }
            ),
        ),
    }

    let t = Instant::now();
    match crate::census::stormblock_health(&engine.http, &engine.base).await {
        Ok(h) => {
            let (st, d) = boot_source(&h);
            out.emit(Line::new("boot-source", st, t.elapsed(), d));
        }
        Err(e) => out.failure("boot-source", t.elapsed(), &e, format!("stormblock health: {e:#}")),
    }
}

/// `boot-source` (#5): what the node runs from, by stormblock's health
/// (`slabs`, stormblock#322; `flow_over_remaining`). Every mode passes: the
/// line records which one the run tested (the install gate judges whether
/// it was the intended one, stormcentral#353). Health without `slabs` fails:
/// the mode cannot be told.
fn boot_source(health: &serde_json::Value) -> (Status, String) {
    let Some(slabs) = health.get("slabs").filter(|s| s.is_object()) else {
        return (Status::Fail, "stormblock health has no `slabs` (stormblock#322): the boot mode cannot be told".into());
    };
    let system = slabs["system"].as_str().unwrap_or("?");
    let data = slabs["data"].as_str().unwrap_or("?");
    let left = health["flow_over_remaining"].as_u64().unwrap_or(0);
    let transports: Vec<&str> = slabs["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["source"].as_str() == Some("remote"))
        .filter_map(|i| i["transport"].as_str())
        .fold(Vec::new(), |mut v, t| {
            if !v.contains(&t) {
                v.push(t);
            }
            v
        });
    let over = if transports.is_empty() { String::new() } else { format!(" over {}", transports.join("+")) };
    let disk = &slabs["local_disk"];
    let disk = match disk["state"].as_str() {
        Some(state) if state != "taken" => {
            let mut d = format!("; own disk {state}");
            if let Some(drive) = disk["drive"].as_str() {
                d += &format!(" ({drive})");
            }
            if let Some(why) = disk["reason"].as_str() {
                d += &format!(": {why}");
            }
            d
        }
        _ => String::new(),
    };
    let mode = if slabs["diskless"].as_bool() == Some(true) {
        format!("diskless: runs from the network{over}")
    } else if left > 0 || system == "mixed" || data == "mixed" {
        format!("flow-over under way: {left} left{over}")
    } else if system == "local" {
        "local disk".to_string()
    } else {
        format!("system half {system}{over}")
    };
    (Status::Pass, format!("{mode} (slabs.system {system}, data {data}){disk}"))
}

/// The label stormcos's reporter puts on every node's ssh report (stormcos#521).
const SSH_KEYS_LABEL: &str = "storm.io/ssh-keys";

/// The key files sshd must read (`AuthorizedKeysFile` in stormcos's fedora
/// golden): the build's baked keys and install-config's `sshKey`.
const SSH_KEY_FILES: [&str; 2] = [".ssh/authorized_keys", "/state/config/ssh/authorized_keys"];

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Seconds since the epoch of an RFC 3339 UTC time as the reporter writes it
/// (`date -u +%Y-%m-%dT%H:%M:%SZ`; fractions and `+00:00` accepted).
fn rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = rest.strip_suffix('Z').or_else(|| rest.strip_suffix("+00:00"))?;
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `ssh-keys` (#66, stormcos#521): for every Ready node, its report
/// `kube-system/ssh-keys-<node>` exists and is fresher than `max_age`, sshd
/// listens on :22, sshd reads both key files, and every `expected`
/// fingerprint is among the baked or install-config keys (none expected:
/// at least one baked key). No report on any node is a release without
/// stormcos#521: skipped as "not carried", not failed.
fn ssh_keys(nodes: &[serde_json::Value], cms: &[serde_json::Value], expected: &[String], now: i64, max_age: u64) -> (Status, String) {
    let ready: Vec<&str> = nodes
        .iter()
        .filter(|n| n["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")))
        .filter_map(|n| n["metadata"]["name"].as_str())
        .collect();
    if ready.is_empty() {
        return (Status::Skip, "no Ready Node to check".into());
    }
    if cms.is_empty() {
        return (Status::Skip, format!("not carried: no ConfigMap kube-system/ssh-keys-<node> ({SSH_KEYS_LABEL}=true) on any node; this release has no stormcos#521 reporter"));
    }
    let expected: Vec<&str> = expected.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let (mut bad, mut good) = (Vec::new(), Vec::new());
    for node in &ready {
        let name = format!("ssh-keys-{node}");
        let Some(cm) = cms.iter().find(|c| c["metadata"]["name"].as_str() == Some(name.as_str()) || c["data"]["node"].as_str() == Some(*node)) else {
            bad.push(format!("{node}: no ConfigMap kube-system/{name}"));
            continue;
        };
        let data = &cm["data"];
        let field = |k: &str| data[k].as_str().unwrap_or("");
        let mut why = Vec::new();
        match rfc3339_utc(field("updated")) {
            None => why.push(format!("updated {:?} is not a time", field("updated"))),
            Some(at) if now - at > max_age as i64 => why.push(format!("updated {} ({} s ago, over {max_age} s): the reporter stopped", field("updated"), now - at)),
            Some(_) => {}
        }
        if field("sshd").trim() != "listening on :22" {
            let holder = field("port-22-holder");
            why.push(format!("sshd {:?}{}", field("sshd").trim(), if holder.is_empty() { String::new() } else { format!(", :22 held by {holder}") }));
        }
        let files: Vec<&str> = field("authorized-keys-files").split_whitespace().collect();
        let missing: Vec<&str> = SSH_KEY_FILES.iter().copied().filter(|f| !files.contains(f)).collect();
        if !missing.is_empty() {
            why.push(format!("sshd does not read {} (AuthorizedKeysFile {:?})", missing.join(", "), field("authorized-keys-files").trim()));
        }
        let fps = |k: &str| -> Vec<String> { field(k).lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect() };
        let (baked, install) = (fps("baked"), fps("install-config"));
        if expected.is_empty() {
            if baked.is_empty() {
                why.push("no baked key (fedora golden's /root/.ssh/authorized_keys is empty)".into());
            }
        } else {
            let absent: Vec<&str> = expected.iter().copied().filter(|e| !baked.iter().chain(&install).any(|f| f == e)).collect();
            if !absent.is_empty() {
                why.push(format!("expected key(s) missing: {}", absent.join(" ")));
            }
        }
        if why.is_empty() {
            good.push(format!("{node}: {} baked + {} install-config key(s)", baked.len(), install.len()));
        } else {
            bad.push(format!("{node}: {}", why.join("; ")));
        }
    }
    let against = if expected.is_empty() { "no expected list: baked keys present".to_string() } else { format!("{} expected key(s) present", expected.len()) };
    if bad.is_empty() {
        (Status::Pass, format!("{} Ready node(s), sshd on :22 reading both key files, {against}; {}", ready.len(), good.join("; ")))
    } else {
        (Status::Fail, format!("{} of {} Ready node(s): {}", bad.len(), ready.len(), bad.join("; ")))
    }
}

/// A cluster-wide list, or why it could not be read (a 403 means the run
/// was not granted the read).
async fn list(kube: &Client, path: &str) -> Result<Vec<serde_json::Value>, String> {
    match kube.get(path).await {
        Ok(r) if r.ok() => Ok(kube::items(&r.body)),
        Ok(r) => Err(format!("{path} answered {} (requires.toml [short] cluster_read)", r.code)),
        Err(e) => Err(format!("{path}: {e:#}")),
    }
}

/// `node-ready`: every Node has `Ready=True`; the detail names the ones
/// that don't, with their conditions.
fn node_ready(nodes: &[serde_json::Value]) -> (Status, String) {
    if nodes.is_empty() {
        return (Status::Fail, "the cluster lists no Nodes".into());
    }
    let mut bad = Vec::new();
    for n in nodes {
        let conds = n["status"]["conditions"].as_array().cloned().unwrap_or_default();
        let ready = conds.iter().any(|c| c["type"] == "Ready" && c["status"] == "True");
        if !ready {
            let cs: Vec<String> = conds
                .iter()
                .map(|c| {
                    let reason = c["reason"].as_str().unwrap_or("");
                    format!("{}={}{}", c["type"].as_str().unwrap_or("?"), c["status"].as_str().unwrap_or("?"), if reason.is_empty() { String::new() } else { format!("({reason})") })
                })
                .collect();
            bad.push(format!("{}: {}", n["metadata"]["name"].as_str().unwrap_or("?"), if cs.is_empty() { "no conditions".into() } else { cs.join(" ") }));
        }
    }
    if bad.is_empty() {
        (Status::Pass, format!("{} Node(s) Ready", nodes.len()))
    } else {
        (Status::Fail, format!("{} of {} Node(s) not Ready: {}", bad.len(), nodes.len(), bad.join("; ")))
    }
}

/// `system-pods`: every platform pod is Running with all its containers
/// Ready, or Succeeded, and none restarted more than `max_restarts` times.
/// Platform pods are those outside the runner's test namespaces (labelled
/// `storm.io/purpose=test`) and not labelled `storm.io/test-run` themselves:
/// on a test machine the rest are the release's, kube-system among them.
fn system_pods(namespaces: &[serde_json::Value], pods: &[serde_json::Value], max_restarts: u64) -> (Status, String) {
    let test_ns: std::collections::HashSet<&str> = namespaces
        .iter()
        .filter(|n| n["metadata"]["labels"]["storm.io/purpose"] == "test" || n["metadata"]["labels"]["storm.io/test-run"].is_string())
        .filter_map(|n| n["metadata"]["name"].as_str())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let (mut total, mut bad) = (0, Vec::new());
    for p in pods {
        let ns = p["metadata"]["namespace"].as_str().unwrap_or("");
        if test_ns.contains(ns) || p["metadata"]["labels"]["storm.io/test-run"].is_string() {
            continue;
        }
        total += 1;
        seen.insert(ns.to_string());
        let st = &p["status"];
        let phase = st["phase"].as_str().unwrap_or("Unknown");
        let cs: Vec<&serde_json::Value> = ["containerStatuses", "initContainerStatuses"]
            .iter()
            .flat_map(|k| st[*k].as_array().map(|a| a.iter().collect::<Vec<_>>()).unwrap_or_default())
            .collect();
        let restarts: u64 = cs.iter().filter_map(|c| c["restartCount"].as_u64()).sum();
        let main: Vec<&serde_json::Value> = st["containerStatuses"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        let all_ready = !main.is_empty() && main.iter().all(|c| c["ready"] == true);
        let ok = match phase {
            "Succeeded" => true,
            "Running" => all_ready,
            _ => false,
        } && restarts <= max_restarts;
        if !ok {
            let reason = main
                .iter()
                .find_map(|c| c["state"]["waiting"]["reason"].as_str().or(c["state"]["terminated"]["reason"].as_str()))
                .or(st["reason"].as_str())
                .unwrap_or("");
            let ready = main.iter().filter(|c| c["ready"] == true).count();
            bad.push(format!(
                "{ns}/{} {phase}{} ready {ready}/{} restarts {restarts}",
                p["metadata"]["name"].as_str().unwrap_or("?"),
                if reason.is_empty() { String::new() } else { format!(" ({reason})") },
                main.len()
            ));
        }
    }
    let where_ = seen.into_iter().collect::<Vec<_>>().join(",");
    if bad.is_empty() {
        (Status::Pass, format!("{total} platform pod(s) up in [{where_}] (restarts ≤ {max_restarts})"))
    } else {
        (Status::Fail, format!("{} of {total} platform pod(s) not up (restarts > {max_restarts} counts): {}", bad.len(), bad.join("; ")))
    }
}

async fn helper(kube: &Client, pods: &str, name: &str, image: &str, run: &str) -> (Status, String) {
    let body = json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "labels": {"storm.io/test-run": run}},
        "spec": {"restartPolicy": "Never", "terminationGracePeriodSeconds": 1, "automountServiceAccountToken": false,
                 "containers": [{"name": "c", "image": image, "command": ["/test"], "args": ["serve"]}]}
    });
    match kube.post(pods, &body).await {
        Ok(r) if r.ok() => {}
        Ok(r) => return (Status::Fail, format!("creating {name} answered {}: {}", r.code, r.body)),
        Err(e) => return (Status::Fail, format!("{e:#}")),
    }
    let deadline = Instant::now() + Duration::from_secs(75);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(r) = kube.get(&format!("{pods}/{name}")).await {
            let s = &r.body["status"];
            if let (Some("Running"), Some(ip)) = (s["phase"].as_str(), s["podIP"].as_str()) {
                let got = async {
                    let mut c = tokio::net::TcpStream::connect((ip, SERVE_PORT)).await?;
                    let mut buf = String::new();
                    c.read_to_string(&mut buf).await?;
                    anyhow::Ok(buf)
                };
                match tokio::time::timeout(Duration::from_secs(5), got).await {
                    Ok(Ok(b)) if b.starts_with("stormcos_qa") => return (Status::Pass, format!("{name} at {ip}:{SERVE_PORT} answered")),
                    Ok(Ok(b)) => last = format!("{ip}:{SERVE_PORT} answered {b:?}"),
                    Ok(Err(e)) => last = format!("{ip}:{SERVE_PORT}: {e}"),
                    Err(_) => last = format!("{ip}:{SERVE_PORT}: no answer"),
                }
            } else {
                last = format!("phase {:?}", s["phase"].as_str());
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    (Status::Fail, format!("{name} did not come up and answer in 75s: {last}"))
}

/// `node-identity`: every Node has a real name (not localhost) and a global
/// IPv4 InternalIP (the old node-hostname and node-has-ip scripts).
fn node_identity(nodes: &[serde_json::Value]) -> (Status, String) {
    if nodes.is_empty() {
        return (Status::Fail, "the cluster lists no Nodes".into());
    }
    let mut bad = Vec::new();
    let mut good = Vec::new();
    for n in nodes {
        let name = n["metadata"]["name"].as_str().unwrap_or("");
        let ip = n["status"]["addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["type"] == "InternalIP")
            .filter_map(|a| a["address"].as_str()?.parse::<std::net::Ipv4Addr>().ok())
            .find(|ip| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified());
        match ip {
            _ if name.is_empty() || name.starts_with("localhost") => bad.push(format!("{name:?}: not a node name")),
            None => bad.push(format!("{name}: no global IPv4 InternalIP")),
            Some(ip) => good.push(format!("{name}={ip}")),
        }
    }
    if bad.is_empty() { (Status::Pass, good.join(" ")) } else { (Status::Fail, bad.join("; ")) }
}

/// `node-stack`: every boot service stormpump runs is running, or is a
/// one-shot that exited 0, and none restarted more than `max_restarts`
/// times (the old boot-to-multi-user and CRI-O scripts; stormpump is PID 1).
fn node_stack(assets: &str, max_restarts: u64) -> (Status, String) {
    let v: serde_json::Value = match serde_json::from_str(assets) {
        Ok(v) => v,
        Err(e) => return (Status::Fail, format!("assets.json does not parse: {e}")),
    };
    let list = v["assets"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return (Status::Fail, "stormpump reports no boot services".into());
    }
    let mut bad = Vec::new();
    for a in &list {
        let name = a["name"].as_str().unwrap_or("?");
        let running = a["running"] == true;
        let done = !running && a["last_exit_code"] == 0;
        let restarts = a["restarts"].as_u64().unwrap_or(0);
        if !(running || done) {
            let why = a["last_error"].as_str().or(a["last_exit"].as_str()).unwrap_or("not running");
            bad.push(format!("{name}: {why}"));
        } else if restarts > max_restarts {
            bad.push(format!("{name}: {restarts} restarts"));
        }
    }
    if bad.is_empty() {
        (Status::Pass, format!("{} boot services up (restarts ≤ {max_restarts})", list.len()))
    } else {
        (Status::Fail, format!("{} of {} boot services not up: {}", bad.len(), list.len(), bad.join("; ")))
    }
}

/// `root`: the host's `/` is a ublk device mounted read-only, any filesystem
/// (stormcos#470: an ext4 clone of the sealed stormpump golden, mounted `ro`;
/// no longer erofs), from PID 1's mountinfo (the old ublk-root-erofs and
/// ublk-devices scripts).
fn root_mount(mountinfo: &str) -> (Status, String) {
    // `<id> <parent> <maj:min> <root> <mount point> <opts> [optional…] - <fstype> <source> <super opts>`
    let root = mountinfo.lines().rfind(|l| l.split(' ').nth(4) == Some("/"));
    let Some(l) = root else { return (Status::Fail, "no / in PID 1's mountinfo".into()) };
    let Some((before, after)) = l.split_once(" - ") else { return (Status::Fail, format!("unreadable mountinfo line {l:?}")) };
    let opts = before.split(' ').nth(5).unwrap_or("");
    let mut f = after.split(' ');
    let (fstype, source) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
    let ro = opts.split(',').any(|o| o == "ro");
    match (source.starts_with("/dev/ublkb"), ro) {
        (true, true) => (Status::Pass, format!("/ is {fstype} on {source}, read-only")),
        (true, false) => (Status::Fail, format!("/ is {fstype} on {source}, mounted rw ({opts}); stormcos#470")),
        (false, _) => (Status::Fail, format!("/ is {fstype} on {source} ({opts}), want a read-only /dev/ublkb*")),
    }
}

/// Tier-0 on the host (stormcos: stormpump volume `stormcertdata`), as the
/// node's login container mounts it read-only at the same path.
const TIER0: &str = "/data/stormcert";

/// The cluster entry `/etc/profile.d/sc.sh` writes into `~/.sc/config.toml`
/// (stormcos `deploy/build-goldens.sh`, checked there by
/// `deploy/test/check-sc-login.sh`): context `node`, the apiserver on the
/// node's loopback, tier-0's CA, the token's value.
struct ScCluster {
    server: &'static str,
    ca: String,
}

fn sc_cluster() -> ScCluster {
    ScCluster { server: "https://127.0.0.1:6443", ca: format!("{TIER0}/ca.crt") }
}

/// What `sc.sh` accepts as a token, and a JWT: base64url and dots only
/// (sc.sh refuses anything else rather than paste it into TOML), three
/// non-empty parts, a JSON header (`eyJ` is `{"` in base64url).
fn jwt_shaped(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("node-admin.token is empty".into());
    }
    if !token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err("node-admin.token has characters outside base64url and '.' (sc.sh would not use it)".into());
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("node-admin.token is not a JWT: {} dot-separated part(s), want 3 non-empty", parts.len()));
    }
    if !parts[0].starts_with("eyJ") {
        return Err("node-admin.token is not a JWT: its header is not base64url JSON".into());
    }
    Ok(())
}

/// The Node that is this machine: `node` (STORM_NODE) by name or by any
/// address, else the only Node.
fn this_node<'a>(nodes: &'a [serde_json::Value], node: &str) -> Option<&'a serde_json::Value> {
    let named = nodes.iter().find(|n| {
        n["metadata"]["name"].as_str() == Some(node)
            || n["status"]["addresses"].as_array().into_iter().flatten().any(|a| a["address"].as_str() == Some(node))
    });
    named.or(if nodes.len() == 1 { nodes.first() } else { None })
}

/// `sc-login` (#59): a login on the node gets a working `sc`. Tier-0's
/// `node-admin.token` exists (stormcert-node-admin minted it), is 0600 and a
/// JWT; a client configured exactly as `sc.sh` configures `sc` — only
/// `ca.crt` trusted, no insecure flag, the apiserver at 127.0.0.1:6443 —
/// lists the Nodes, and this node is Ready. Each failure names the part
/// that broke: no token, a CA mismatch, auth refused. `Skip` here means the
/// check could not run (the files are not mounted, or the Job is not on the
/// host network, so 127.0.0.1 is not the node's).
/// The engine's record of every destructive call (stormblock `mgmt/kubeauth.rs`
/// `AuditRecord`). It goes nowhere else: no API, no journal.
const ENGINE_AUDIT_LOG: &str = "/run/stormblock/engine/audit.log";
/// How many of its records one run prints, blockers first.
const ENGINE_AUDIT_PRINTED: usize = 500;
/// How many blockers the result line names.
const ENGINE_AUDIT_NAMED: usize = 10;

/// A node service's own identity: a refusal of one of these is the node's
/// storage failing, where a refused `none` or stranger is the gate working.
fn node_identity_who(who: &str) -> bool {
    who == "node-token" || who.starts_with("client-cert:") || who.starts_with("kubernetes:system:serviceaccount:kube-system:")
}

/// `engine-audit` (#79): judge the log's text. Fail on a call that enforcing
/// would refuse (`allowed-audit-only`, by the node token or a node
/// certificate alike) and on a node service already refused. Skip ("could not
/// run") when the log cannot vouch: no destructive call allowed in it, or a
/// line that cannot be judged. Also returns the records to print, blockers
/// first. `served-plain` records (stormblock#525) share the file but are not
/// destructive calls.
fn engine_audit(text: &str) -> (Status, String, Vec<String>) {
    let (mut blockers, mut others, mut allowed) = (Vec::new(), Vec::new(), std::collections::BTreeMap::<String, u64>::new());
    let (mut unreadable, mut plain, mut refused) = (0u64, 0u64, 0u64);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        // The engine writes a record and its newline separately, so two
        // callers at once can leave two records on one line.
        for rec in serde_json::Deserializer::from_str(line).into_iter::<serde_json::Value>() {
            let Ok(rec) = rec else {
                // Torn. Not knowing who it was, it can only hurt.
                if line.contains("allowed-audit-only") {
                    blockers.push(format!("torn record: {line}"));
                } else {
                    unreadable += 1;
                }
                break;
            };
            let field = |k: &str| rec[k].as_str().unwrap_or("?").to_string();
            let (who, decision) = (field("who"), field("decision"));
            let call = format!("{} {}", field("method"), field("path"));
            let shown = format!("{who} {decision} {call}");
            match decision.as_str() {
                "served-plain" => plain += 1,
                "allowed-audit-only" => blockers.push(shown),
                "refused" if node_identity_who(&who) => blockers.push(shown),
                "refused" => {
                    refused += 1;
                    others.push(shown);
                }
                "allowed" => {
                    *allowed.entry(who).or_default() += 1;
                    others.push(shown);
                }
                _ => {
                    unreadable += 1;
                    others.push(shown);
                }
            }
        }
    }
    let total: u64 = allowed.values().sum();
    let counts = allowed.iter().map(|(w, n)| format!("{w} ×{n}")).collect::<Vec<_>>().join(", ");
    let (status, detail) = if !blockers.is_empty() {
        let named = blockers.iter().take(ENGINE_AUDIT_NAMED).cloned().collect::<Vec<_>>().join("; ");
        let more = blockers.len().saturating_sub(ENGINE_AUDIT_NAMED);
        let more = if more > 0 { format!("; and {more} more") } else { String::new() };
        (Status::Fail, format!("{} destructive call(s) enforcing would refuse, or a node service refused: {named}{more}", blockers.len()))
    } else if unreadable > 0 {
        (Status::Skip, format!("{unreadable} record(s) that cannot be judged, beside {total} allowed"))
    } else if total == 0 {
        (Status::Skip, format!("no destructive call allowed in the log ({refused} refused, {plain} plain requests): it proves nothing either way"))
    } else {
        (Status::Pass, format!("{total} destructive call(s) allowed, none by audit mode alone: {counts} ({refused} outsiders refused)"))
    };
    let mut records = blockers;
    records.append(&mut others);
    let more = records.len().saturating_sub(ENGINE_AUDIT_PRINTED);
    records.truncate(ENGINE_AUDIT_PRINTED);
    if more > 0 {
        records.push(format!("and {more} more not printed"));
    }
    (status, detail, records)
}

async fn sc_login(host: &str, node: &str) -> (Status, String) {
    use std::os::unix::fs::MetadataExt;
    if host.is_empty() {
        return (Status::Skip, format!("no STORM_HOST_ROOT: {TIER0}/node-admin.token and ca.crt not mounted (requires.toml [short] host_paths_read_only)"));
    }
    let cfg = sc_cluster();
    let (token_path, ca_path) = (format!("{host}{TIER0}/node-admin.token"), format!("{host}{}", cfg.ca));
    let ca = std::fs::read(&ca_path).ok().filter(|c| !c.is_empty());
    let token_meta = std::fs::metadata(&token_path).ok().filter(|m| m.is_file());
    let Some(ca) = ca else {
        return match token_meta {
            None => (Status::Skip, format!("neither {TIER0}/ca.crt nor node-admin.token under {host} (requires.toml [short] host_paths_read_only)")),
            Some(_) => (Status::Fail, format!("no CA: {TIER0}/ca.crt is missing or empty on the node (stormcert-init)")),
        };
    };
    let Some(meta) = token_meta else {
        return (Status::Fail, format!("no token: {TIER0}/node-admin.token is not on the node (stormcert-node-admin did not mint it)"));
    };
    let mode = meta.mode() & 0o777;
    if mode != 0o600 {
        return (Status::Fail, format!("{TIER0}/node-admin.token is mode {mode:04o}, want 0600"));
    }
    let token = match std::fs::read_to_string(&token_path) {
        Ok(t) => t.split_whitespace().collect::<String>(),
        Err(e) => return (Status::Skip, format!("{token_path}: {e}")),
    };
    if let Err(why) = jwt_shaped(&token) {
        return (Status::Fail, why);
    }
    let netdev = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    if !crate::census::host_netns(&netdev, "stormbr0") {
        return (Status::Skip, format!("not on the host network, so {} is not the node's (requires.toml [short] host_network)", cfg.server));
    }
    let kube = match Client::with_ca(cfg.server, &token_path, &ca) {
        Ok(k) => k,
        Err(e) => return (Status::Fail, format!("CA: {TIER0}/ca.crt: {e:#}")),
    };
    match kube.get("/api/v1/nodes").await {
        Ok(r) if r.ok() => {
            let nodes = kube::items(&r.body);
            let Some(n) = this_node(&nodes, node) else {
                let names: Vec<&str> = nodes.iter().filter_map(|n| n["metadata"]["name"].as_str()).collect();
                return (Status::Fail, format!("as node-admin with ca.crt: {} lists {} Node(s) [{}], none is {node}", cfg.server, nodes.len(), names.join(",")));
            };
            let name = n["metadata"]["name"].as_str().unwrap_or("?");
            match node_ready(std::slice::from_ref(n)) {
                (Status::Pass, _) => (Status::Pass, format!("as node-admin (token 0600, JWT) with only {TIER0}/ca.crt trusted: {} lists {name} Ready", cfg.server)),
                (_, d) => (Status::Fail, format!("as node-admin with ca.crt: {} lists {name}, not Ready: {d}", cfg.server)),
            }
        }
        Ok(r) if r.code == 401 || r.code == 403 => (Status::Fail, format!("auth refused: {} answered {} to node-admin's token: {}", cfg.server, r.code, r.body)),
        Ok(r) => (Status::Fail, format!("{} answered {} to GET /api/v1/nodes: {}", cfg.server, r.code, r.body)),
        Err(e) => {
            let why = format!("{e:#}");
            if retry::http::certificate_refused(&why) {
                (Status::Fail, format!("CA mismatch: the apiserver's certificate does not verify against {TIER0}/ca.crt for 127.0.0.1: {why}"))
            } else if retry::is_infra(&e) {
                (Status::Fail, format!("the apiserver did not answer at {} (a login's sc would fail the same way): {why}", cfg.server))
            } else {
                (Status::Fail, why)
            }
        }
    }
}

/// `ssh`: the node answers on :22 with an SSH banner (the old ssh-reachable
/// script; ssh lands in the node's `fedora` container).
async fn ssh_banner(node: &str) -> (Status, String) {
    // No answer is tried again (#57: retry::Policy::PROBE); the last try decides.
    let (got, _) = retry::probe(&retry::Policy::PROBE, &format!("ssh banner {node}:22"), |_| async {
        let got = async {
            let mut c = tokio::net::TcpStream::connect((node, 22)).await?;
            let mut buf = vec![0u8; 256];
            let n = c.read(&mut buf).await?;
            anyhow::Ok(String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string())
        };
        let r = tokio::time::timeout(Duration::from_secs(10), got).await;
        let answered = matches!(r, Ok(Ok(_)));
        (r, answered)
    })
    .await;
    match got {
        Ok(Ok(b)) if b.starts_with("SSH-") => (Status::Pass, format!("{node}:22 answered {b}")),
        Ok(Ok(b)) => (Status::Fail, format!("{node}:22 answered {b:?}, not an SSH banner")),
        Ok(Err(e)) => (Status::Fail, format!("{node}:22: {e}")),
        Err(_) => (Status::Fail, format!("{node}:22: no banner in 10 s")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn audit_line(who: &str, decision: &str, method: &str, path: &str) -> String {
        json!({"at": 1, "who": who, "method": method, "path": path, "resource": "slabs", "verb": "destroy", "decision": decision}).to_string()
    }

    #[test]
    fn engine_audit_fails_on_a_call_enforcing_would_refuse() {
        let log = [
            audit_line("kubernetes:system:serviceaccount:kube-system:stormdrive", "allowed", "POST", "/api/v1/slabs/format"),
            audit_line("node-token", "allowed-audit-only", "DELETE", "/api/v1/arrays/a1"),
        ]
        .join("\n");
        let (st, d, records) = engine_audit(&log);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("node-token allowed-audit-only DELETE /api/v1/arrays/a1"), "{d}");
        assert!(records[0].starts_with("node-token"), "blockers print first: {records:?}");
        assert_eq!(records.len(), 2);
        // A node certificate sits at the node token's tier: enforce refuses it too.
        let (st, d, _) = engine_audit(&audit_line("client-cert:ab12", "allowed-audit-only", "POST", "/api/v1/forge/on"));
        assert_eq!(st, Status::Fail);
        assert!(d.contains("client-cert:ab12"), "{d}");
    }

    #[test]
    fn engine_audit_passes_on_allowed_identities_and_counts_them() {
        let sa = "kubernetes:system:serviceaccount:kube-system:stormcluster";
        let log = [
            audit_line(sa, "allowed", "POST", "/api/v1/forge/on"),
            audit_line(sa, "allowed", "POST", "/api/v1/forge/off"),
            audit_line("admin-token", "allowed", "POST", "/api/v1/slabs/format"),
            audit_line("client-cert-admin:forge:ab", "allowed", "DELETE", "/api/v1/goldens/x"),
            audit_line("none", "refused", "DELETE", "/api/v1/goldens/x"),
        ]
        .join("\n");
        let (st, d, records) = engine_audit(&log);
        assert_eq!(st, Status::Pass, "{d}");
        assert!(d.contains(&format!("{sa} ×2")), "{d}");
        assert!(d.contains("admin-token ×1"), "{d}");
        assert!(d.contains("1 outsiders refused"), "{d}");
        assert_eq!(records.len(), 5, "every destructive record prints");
    }

    #[test]
    fn engine_audit_fails_when_a_node_service_is_refused() {
        // After enforcing, a node service left on the node token reads this way.
        for who in ["node-token", "kubernetes:system:serviceaccount:kube-system:stormdrive"] {
            let (st, d, _) = engine_audit(&audit_line(who, "refused", "POST", "/api/v1/slabs/format"));
            assert_eq!(st, Status::Fail, "{who}: {d}");
        }
    }

    #[test]
    fn engine_audit_cannot_vouch_for_a_log_with_no_allowed_call() {
        // Plain-request records share the file and are no destructive call,
        // even with an allowed-looking caller.
        for log in [
            String::new(),
            "\n\n".to_string(),
            audit_line("node-token", "served-plain", "GET", "/api/v1/volumes"),
            audit_line("none", "refused", "DELETE", "/api/v1/goldens/x"),
        ] {
            let (st, d, _) = engine_audit(&log);
            assert_eq!(st, Status::Skip, "{log:?}: {d}");
            assert!(d.contains("proves nothing"), "{d}");
        }
    }

    #[test]
    fn engine_audit_never_passes_past_what_it_cannot_read() {
        let good = audit_line("admin-token", "allowed", "POST", "/api/v1/slabs/format");
        for bad in ["{\"who\":\"node-to", "\0\0\0", "{\"who\":\"x\",\"decision\":\"new-kind\"}"] {
            let (st, d, _) = engine_audit(&format!("{good}\n{bad}"));
            assert_eq!(st, Status::Skip, "{bad:?}: {d}");
            assert!(d.contains("cannot be judged"), "{d}");
        }
        // Torn, but what it says is still enough.
        let (st, _, _) = engine_audit(&format!("{good}\n{{\"who\":\"node-token\",\"decision\":\"allowed-audit-only\",\"meth"));
        assert_eq!(st, Status::Fail);
    }

    #[test]
    fn engine_audit_reads_two_records_on_one_line() {
        // Two callers at once: the engine's two writes interleave.
        let log = format!("{}{}\n\n", audit_line("admin-token", "allowed", "POST", "/a"), audit_line("node-token", "allowed-audit-only", "POST", "/b"));
        let (st, d, _) = engine_audit(&log);
        assert_eq!(st, Status::Fail, "{d}");
        assert!(d.contains("POST /b"), "{d}");
    }

    #[test]
    fn engine_audit_caps_what_it_prints_and_names() {
        let log = (0..ENGINE_AUDIT_PRINTED + 5).map(|i| audit_line("node-token", "allowed-audit-only", "POST", &format!("/p{i}"))).collect::<Vec<_>>().join("\n");
        let (st, d, records) = engine_audit(&log);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("and 495 more"), "{d}");
        assert_eq!(records.len(), ENGINE_AUDIT_PRINTED + 1);
        assert_eq!(records[ENGINE_AUDIT_PRINTED], "and 5 more not printed");
    }

    fn node(name: &str, ready: &str) -> serde_json::Value {
        json!({"metadata": {"name": name}, "status": {"conditions": [
            {"type": "MemoryPressure", "status": "False"},
            {"type": "Ready", "status": ready, "reason": "KubeletNotReady"}]}})
    }

    fn pod(ns: &str, name: &str, phase: &str, ready: bool, restarts: u64) -> serde_json::Value {
        json!({"metadata": {"namespace": ns, "name": name}, "status": {"phase": phase,
            "containerStatuses": [{"name": "c", "ready": ready, "restartCount": restarts, "state": {"running": {}}}]}})
    }

    #[test]
    fn node_ready_names_the_unready_with_conditions() {
        assert_eq!(node_ready(&[node("a", "True")]).0, Status::Pass);
        let (st, d) = node_ready(&[node("a", "True"), node("b", "False")]);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("1 of 2") && d.contains("b: MemoryPressure=False Ready=False(KubeletNotReady)"), "{d}");
        assert!(!d.contains("a:"), "{d}");
        assert_eq!(node_ready(&[]).0, Status::Fail);
    }

    #[test]
    fn system_pods_skips_test_namespaces_and_counts_restarts() {
        let nss = [
            json!({"metadata": {"name": "kube-system"}}),
            json!({"metadata": {"name": "test-x-short-r1", "labels": {"storm.io/purpose": "test", "storm.io/test-run": "r1"}}}),
        ];
        let mut pods = vec![
            pod("kube-system", "cilium-1", "Running", true, 0),
            json!({"metadata": {"namespace": "kube-system", "name": "job-1"}, "status": {"phase": "Succeeded", "containerStatuses": [{"ready": false, "restartCount": 0}]}}),
            pod("test-x-short-r1", "broken", "Pending", false, 9),
        ];
        let (st, d) = system_pods(&nss, &pods, 3);
        assert_eq!(st, Status::Pass, "{d}");
        assert!(d.starts_with("2 platform pod(s) up in [kube-system]"), "{d}");

        // Running and Ready, but crash-looping: the case that hides.
        pods.push(pod("kube-system", "dns-1", "Running", true, 4));
        pods.push(json!({"metadata": {"namespace": "stormvm", "name": "vm-op"}, "status": {"phase": "Running",
            "containerStatuses": [{"ready": false, "restartCount": 1, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}}));
        let (st, d) = system_pods(&nss, &pods, 3);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("kube-system/dns-1 Running ready 1/1 restarts 4"), "{d}");
        assert!(d.contains("stormvm/vm-op Running (CrashLoopBackOff) ready 0/1 restarts 1"), "{d}");
        assert!(!d.contains("broken"), "{d}");
    }

    #[test]
    fn node_identity_wants_a_name_and_a_global_ipv4() {
        let n = |name: &str, ip: &str| json!({"metadata": {"name": name}, "status": {"addresses": [{"type": "InternalIP", "address": ip}, {"type": "Hostname", "address": name}]}});
        assert_eq!(node_identity(&[n("storm-06f96d", "192.168.30.2")]).0, Status::Pass);
        assert!(node_identity(&[n("localhost.localdomain", "192.168.30.2")]).1.contains("not a node name"));
        assert!(node_identity(&[n("n1", "127.0.0.1")]).1.contains("no global IPv4"));
        assert!(node_identity(&[n("n1", "169.254.1.1")]).1.contains("no global IPv4"));
    }

    #[test]
    fn node_stack_counts_one_shots_done_and_restarts() {
        let a = r#"{"assets":[
            {"name":"fastetcd","running":true,"restarts":0},
            {"name":"00-timesync","running":false,"restarts":0,"last_exit_code":0,"last_exit":"exited 0"},
            {"name":"stormvm","running":false,"restarts":2,"last_exit_code":1,"last_exit":"exited 1","last_error":"bind :9095: address in use"},
            {"name":"rustkube-node","running":true,"restarts":7}]}"#;
        let (st, d) = node_stack(a, 3);
        assert_eq!(st, Status::Fail);
        assert!(d.starts_with("2 of 4") && d.contains("stormvm: bind :9095") && d.contains("rustkube-node: 7 restarts"), "{d}");
        assert_eq!(node_stack(r#"{"assets":[{"name":"a","running":true,"restarts":0}]}"#, 3).0, Status::Pass);
        assert_eq!(node_stack(r#"{"assets":[]}"#, 3).0, Status::Fail);
    }

    #[test]
    fn root_is_read_only_ublk_any_fs() {
        let ext4 = "1 0 259:0 / / ro,relatime shared:1 - ext4 /dev/ublkb0 ro\n22 1 0:21 / /proc rw - proc proc rw\n";
        assert_eq!(root_mount(ext4), (Status::Pass, "/ is ext4 on /dev/ublkb0, read-only".into()));
        let erofs = "1 0 259:0 / / ro,relatime shared:1 - erofs /dev/ublkb0 ro,user_xattr\n";
        assert_eq!(root_mount(erofs).0, Status::Pass);
        let rw = "1 0 259:0 / / rw,relatime shared:1 - ext4 /dev/ublkb0 rw\n";
        assert_eq!(root_mount(rw), (Status::Fail, "/ is ext4 on /dev/ublkb0, mounted rw (rw,relatime); stormcos#470".into()));
        // `ro` only in the super options is not a read-only mount
        let super_ro = "1 0 259:0 / / rw,relatime shared:1 - ext4 /dev/ublkb0 ro\n";
        assert_eq!(root_mount(super_ro).0, Status::Fail);
        let overlay = "1 0 0:30 / / ro shared:1 - overlay overlay rw,lowerdir=/l\n";
        assert!(root_mount(overlay).1.contains("/ is overlay on overlay (ro), want a read-only /dev/ublkb*"));
        assert_eq!(root_mount("22 1 0:21 / /proc rw - proc proc rw\n").0, Status::Fail);
    }

    #[test]
    fn sc_cluster_is_what_sc_sh_writes() {
        let c = sc_cluster();
        assert_eq!(c.server, "https://127.0.0.1:6443");
        assert_eq!(c.ca, "/data/stormcert/ca.crt");
    }

    #[test]
    fn jwt_shape_is_checked_like_sc_sh_and_more() {
        // allow-secret: a fake JWT fixture ({"sub":"x"}), as stormcos check-sc-login.sh
        assert!(jwt_shaped("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln-_A1").is_ok());
        assert!(jwt_shaped("").unwrap_err().contains("empty"));
        assert!(jwt_shaped("eyJ\"bad").unwrap_err().contains("base64url"));
        assert!(jwt_shaped("eyJa.b").unwrap_err().contains("2 dot-separated"));
        assert!(jwt_shaped("eyJa..c").unwrap_err().contains("3 non-empty"));
        assert!(jwt_shaped("abc.def.ghi").unwrap_err().contains("header"));
    }

    #[test]
    fn this_node_by_name_address_or_the_only_one() {
        let mut a = node("storm-a", "True");
        a["status"]["addresses"] = json!([{"type": "InternalIP", "address": "192.168.30.2"}]);
        let b = node("storm-b", "True");
        let two = vec![a.clone(), b.clone()];
        assert_eq!(this_node(&two, "storm-b").unwrap()["metadata"]["name"], "storm-b");
        assert_eq!(this_node(&two, "192.168.30.2").unwrap()["metadata"]["name"], "storm-a");
        assert!(this_node(&two, "127.0.0.1").is_none());
        assert_eq!(this_node(&[b], "127.0.0.1").unwrap()["metadata"]["name"], "storm-b");
    }

    #[test]
    fn boot_source_names_local_flow_over_and_diskless() {
        let local = json!({"slabs": {"diskless": false, "system": "local", "data": "local", "items": []}});
        assert_eq!(boot_source(&local), (Status::Pass, "local disk (slabs.system local, data local)".into()));
        let flowing = json!({"flow_over_remaining": 3, "slabs": {"diskless": false, "system": "mixed", "data": "remote",
            "items": [{"role": "system", "source": "local"}, {"role": "data", "source": "remote", "transport": "nvme-tcp"}]}});
        assert_eq!(boot_source(&flowing).1, "flow-over under way: 3 left over nvme-tcp (slabs.system mixed, data remote)");
        let diskless = json!({"slabs": {"diskless": true, "system": "remote", "data": "remote",
            "items": [{"source": "remote", "transport": "nvme-tcp"}, {"source": "remote", "transport": "nvme-tcp"}],
            "local_disk": {"state": "refused", "drive": "sda", "reason": "foreign slabs"}}});
        assert_eq!(
            boot_source(&diskless),
            (Status::Pass, "diskless: runs from the network over nvme-tcp (slabs.system remote, data remote); own disk refused (sda): foreign slabs".into())
        );
        let none = json!({"local_disk": {"state": "none"}});
        assert_eq!(boot_source(&none).0, Status::Fail);
    }

    fn ssh_cm(node: &str, updated: &str, sshd: &str, files: &str, baked: &str, install: &str) -> serde_json::Value {
        json!({"metadata": {"name": format!("ssh-keys-{node}"), "namespace": "kube-system", "labels": {"storm.io/ssh-keys": "true"}},
            "data": {"node": node, "baked": baked, "install-config": install, "sshd": sshd, "port-22-holder": "nobody",
                "authorized-keys-files": files, "updated": updated}})
    }

    #[test]
    fn rfc3339_is_read_as_utc_seconds() {
        assert_eq!(rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_utc("2026-11-03T12:00:00Z"), Some(1_793_707_200));
        assert_eq!(rfc3339_utc("2024-02-29T23:59:59.5+00:00"), Some(1_709_251_199));
        assert_eq!(rfc3339_utc("2026-11-03 12:00:00"), None);
        assert_eq!(rfc3339_utc("-"), None);
    }

    #[test]
    fn ssh_keys_checks_every_ready_node_report() {
        let now = rfc3339_utc("2026-11-03T12:00:00Z").unwrap();
        let files = ".ssh/authorized_keys /state/config/ssh/authorized_keys";
        let baked = "SHA256:aaa ED25519 root@builder\nSHA256:bbb RSA root@dev";
        let nodes = [node("n1", "True"), node("n2", "False")];
        let ok = ssh_cm("n1", "2026-11-03T11:40:00Z", "listening on :22", files, baked, "SHA256:ccc ED25519 install");
        let (st, d) = ssh_keys(&nodes, std::slice::from_ref(&ok), &[], now, 2100);
        assert_eq!(st, Status::Pass, "{d}");
        assert!(d.contains("n1: 2 baked + 1 install-config"), "{d}");
        // Expected keys from either file; a missing one is named.
        let want = vec!["SHA256:aaa".to_string(), "SHA256:ccc".to_string()];
        assert_eq!(ssh_keys(&nodes, std::slice::from_ref(&ok), &want, now, 2100).0, Status::Pass);
        let (st, d) = ssh_keys(&nodes, std::slice::from_ref(&ok), &["SHA256:zzz".to_string()], now, 2100);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("missing: SHA256:zzz"), "{d}");
        // Stale, sshd down, one file not read, no baked keys: each named.
        let bad = ssh_cm("n1", "2026-11-03T11:00:00Z", "not listening on :22", ".ssh/authorized_keys", "", "");
        let (st, d) = ssh_keys(&nodes, &[bad], &[], now, 2100);
        assert_eq!(st, Status::Fail);
        for want in ["3600 s ago", "not listening on :22", "does not read /state/config/ssh/authorized_keys", "no baked key"] {
            assert!(d.contains(want), "{want}: {d}");
        }
        // A Ready node without its report fails once the release carries them.
        let nodes2 = [node("n1", "True"), node("n3", "True")];
        let (st, d) = ssh_keys(&nodes2, std::slice::from_ref(&ok), &[], now, 2100);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("n3: no ConfigMap kube-system/ssh-keys-n3"), "{d}");
        // No report anywhere: a release without stormcos#521.
        let (st, d) = ssh_keys(&nodes, &[], &[], now, 2100);
        assert_eq!(st, Status::Skip);
        assert!(d.starts_with("not carried"), "{d}");
    }

    #[test]
    fn system_pods_ignores_pods_labelled_with_a_run() {
        let pods = [json!({"metadata": {"namespace": "default", "name": "p", "labels": {"storm.io/test-run": "r2"}}, "status": {"phase": "Pending"}})];
        assert_eq!(system_pods(&[], &pods, 3).0, Status::Pass);
    }
}
