//! `/test short` — what `medium` and `long` stand on, in under 2 minutes:
//! the apiserver answers with this run's credentials, every Node is Ready
//! and every platform pod is up (#34, stormcos#51's smoke test), the VirtualMachine
//! resource is served, the Linux golden the VM suites clone is on the node,
//! and a helper pod started from this test's own image comes up and answers
//! (the same way `medium` starts its servers and agents). `sc-login` proves
//! a node login's `sc` works: tier-0's node-admin token and CA reach the
//! apiserver as `/etc/profile.d/sc.sh` configures them (#59, stormcos#60).

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
    /// stormblock's API (default http://<node>:9090).
    #[arg(long)]
    stormblock_url: Option<String>,
    /// A platform pod restarted more often than this fails `system-pods`,
    /// even when it is Running at the moment of the check (#34).
    #[arg(long, default_value_t = 3)]
    max_restarts: u64,
    /// How long `node-ready` and `system-pods` wait for a clean answer
    /// before they report what they see (the whole suite has 120 s).
    #[arg(long, default_value_t = 30)]
    settle: u64,
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
    let (st, d) = ssh_banner(&a.node).await;
    out.emit(Line::new("ssh", st, t.elapsed(), d));

    let t = Instant::now();
    match kube.get(&kube::vms(&ns)).await {
        Ok(r) if r.ok() => out.emit(Line::new("vm-resource", Status::Pass, t.elapsed(), "kubevirt.io/v1 virtualmachines served")),
        Ok(r) => out.emit(Line::new("vm-resource", Status::Fail, t.elapsed(), format!("listing VirtualMachines answered {}: {}", r.code, r.body))),
        Err(e) => out.failure("vm-resource", t.elapsed(), &e, format!("{e:#}")),
    }

    let t = Instant::now();
    let sb = a.stormblock_url.clone().unwrap_or_else(|| format!("http://{}:9090", a.node));
    let token = crate::census::stormblock_token();
    let http = crate::census::stormblock_client(token.as_deref()).expect("stormblock client");
    match crate::census::volume_named(&http, &sb, &a.golden).await {
        Ok(true) => out.emit(Line::new("golden", Status::Pass, t.elapsed(), format!("{} is on the node", a.golden))),
        Ok(false) => out.emit(Line::new("golden", Status::Fail, t.elapsed(), format!("{} is not among the node's stormblock volumes", a.golden))),
        Err(e) => out.failure(
            "golden",
            t.elapsed(),
            &e,
            format!(
                "stormblock refused the volume list ({e:#}; {})",
                if token.is_some() { "token sent" } else { "no token found: STORMBLOCK_API_TOKEN, STORMBLOCK_TOKEN_FILE, /etc/stormblock/api_token or <host>/run/stormblock/engine/api_token" }
            ),
        ),
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
    fn system_pods_ignores_pods_labelled_with_a_run() {
        let pods = [json!({"metadata": {"namespace": "default", "name": "p", "labels": {"storm.io/test-run": "r2"}}, "status": {"phase": "Pending"}})];
        assert_eq!(system_pods(&[], &pods, 3).0, Status::Pass);
    }
}
