//! `/test short` — what `medium` and `long` stand on, in under 2 minutes:
//! the apiserver answers with this run's credentials, the VirtualMachine
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
    let (st, d) = match http.get(format!("{}/api/v1/volumes/{}", sb.trim_end_matches('/'), a.golden)).send().await {
        Ok(r) if r.status().is_success() => (Status::Pass, format!("{} is on the node", a.golden)),
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => (
            Status::Fail,
            format!(
                "stormblock refused the golden lookup ({}; {})",
                r.status(),
                if token.is_some() { "token sent" } else { "no token found: STORMBLOCK_API_TOKEN, STORMBLOCK_TOKEN_FILE, /etc/stormblock/api_token or <host>/run/stormblock/engine/api_token" }
            ),
        ),
        Ok(r) => (Status::Fail, format!("{} not on the node's stormblock ({})", a.golden, r.status())),
        Err(e) => (Status::Fail, format!("stormblock {sb}: {e}")),
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
    if out.failed() > 0 { 1 } else { 0 }
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
