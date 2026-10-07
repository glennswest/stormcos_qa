//! `/test short` — what `medium` and `long` stand on, in under 2 minutes:
//! the apiserver answers with this run's credentials, every Node is Ready
//! and every platform pod is up (#34, stormcos#51's smoke test), the VirtualMachine
//! resource is served, the Linux golden the VM suites clone is on the node,
//! and a helper pod started from this test's own image comes up and answers
//! (the same way `medium` starts its servers and agents).

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
    let mut could_not_run = 0;
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
                out.emit(Line::new(test, Status::Fail, t.elapsed(), format!("could not run: {why}")));
                could_not_run += 1;
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
            out.emit(Line::new("node-identity", Status::Fail, t.elapsed(), format!("could not run: {why}")));
            could_not_run += 1;
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
                out.emit(Line::new(test, Status::Fail, t.elapsed(), format!("could not run: {path}: {e} (requires.toml [short] host_paths_read_only)")));
                could_not_run += 1;
            }
        }
    }
    let t = Instant::now();
    let (st, d) = ssh_banner(&a.node).await;
    out.emit(Line::new("ssh", st, t.elapsed(), d));

    let t = Instant::now();
    let (st, d) = match kube.get(&kube::vms(&ns)).await {
        Ok(r) if r.ok() => (Status::Pass, "kubevirt.io/v1 virtualmachines served".to_string()),
        Ok(r) => (Status::Fail, format!("listing VirtualMachines answered {}: {}", r.code, r.body)),
        Err(e) => (Status::Fail, format!("{e:#}")),
    };
    out.emit(Line::new("vm-resource", st, t.elapsed(), d));

    let t = Instant::now();
    let sb = a.stormblock_url.clone().unwrap_or_else(|| format!("http://{}:9090", a.node));
    let token = crate::census::stormblock_token();
    let http = crate::census::stormblock_client(token.as_deref()).expect("stormblock client");
    let (st, d) = match crate::census::volume_named(&http, &sb, &a.golden).await {
        Ok(true) => (Status::Pass, format!("{} is on the node", a.golden)),
        Ok(false) => (Status::Fail, format!("{} is not among the node's stormblock volumes", a.golden)),
        Err(why) => (
            Status::Fail,
            format!(
                "stormblock refused the volume list ({why}; {})",
                if token.is_some() { "token sent" } else { "no token found: STORMBLOCK_API_TOKEN, STORMBLOCK_TOKEN_FILE, /etc/stormblock/api_token or <host>/run/stormblock/engine/api_token" }
            ),
        ),
    };
    out.emit(Line::new("golden", st, t.elapsed(), d));

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
    if out.failed() > could_not_run {
        1
    } else if could_not_run > 0 {
        2
    } else {
        0
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

/// `root`: the host's `/` is erofs served over ublk (stormcos's boot: "the
/// root it hands over to is an erofs thin volume served over ublk"), from
/// PID 1's mountinfo (the old ublk-root-erofs and ublk-devices scripts).
fn root_mount(mountinfo: &str) -> (Status, String) {
    // `<id> <parent> <maj:min> <root> <mount point> <opts> [optional…] - <fstype> <source> <super opts>`
    let root = mountinfo.lines().filter(|l| l.split(' ').nth(4) == Some("/")).last();
    let Some(l) = root else { return (Status::Fail, "no / in PID 1's mountinfo".into()) };
    let Some((_, after)) = l.split_once(" - ") else { return (Status::Fail, format!("unreadable mountinfo line {l:?}")) };
    let mut f = after.split(' ');
    let (fstype, source) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
    if fstype == "erofs" && source.starts_with("/dev/ublkb") {
        (Status::Pass, format!("/ is erofs on {source}"))
    } else {
        (Status::Fail, format!("/ is {fstype} on {source}, want erofs on /dev/ublkb*"))
    }
}

/// `ssh`: the node answers on :22 with an SSH banner (the old ssh-reachable
/// script; ssh lands in the node's `fedora` container).
async fn ssh_banner(node: &str) -> (Status, String) {
    let got = async {
        let mut c = tokio::net::TcpStream::connect((node, 22)).await?;
        let mut buf = vec![0u8; 256];
        let n = c.read(&mut buf).await?;
        anyhow::Ok(String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string())
    };
    match tokio::time::timeout(Duration::from_secs(10), got).await {
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
    fn root_is_erofs_on_ublk() {
        let good = "1 0 259:0 / / ro,relatime shared:1 - erofs /dev/ublkb0 ro,user_xattr\n22 1 0:21 / /proc rw - proc proc rw\n";
        assert_eq!(root_mount(good), (Status::Pass, "/ is erofs on /dev/ublkb0".into()));
        let overlay = "1 0 0:30 / / rw shared:1 - overlay overlay rw,lowerdir=/l\n";
        assert!(root_mount(overlay).1.contains("/ is overlay on overlay"));
        assert_eq!(root_mount("22 1 0:21 / /proc rw - proc proc rw\n").0, Status::Fail);
    }

    #[test]
    fn system_pods_ignores_pods_labelled_with_a_run() {
        let pods = [json!({"metadata": {"namespace": "default", "name": "p", "labels": {"storm.io/test-run": "r2"}}, "status": {"phase": "Pending"}})];
        assert_eq!(system_pods(&[], &pods, 3).0, Status::Pass);
    }
}
