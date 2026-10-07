//! must-gather — Storm CoreOS debug-data collector (our `oc adm must-gather`),
//! run from a workstation or from a pod. No ssh: a stormcos node has none
//! for this, and its services run under stormpump, not systemd (#45).
//!
//! - **cluster** (`cluster.rs`): through the cluster API (TLS + token):
//!   discovery, health, every listed resource and custom resource, and the
//!   logs of kube-system's pods (the node's own services are mirror pods
//!   there) and of every pod in trouble.
//! - **host** (`host.rs`): per node, a short-lived pod with `hostPID`,
//!   `hostNetwork` and read-only host mounts runs `must-gather
//!   host-collect` from `--host-image`, which sends the node's stormpump and
//!   service logs, kernel log, pstore, host network, stormblock and fastetcd
//!   state back through its pod log (`frame.rs`).
//! - **collectors**: `gather/<area>/*.sh` scripts that components own, run
//!   where must-gather runs with `QA_API`, `QA_TOKEN_FILE`, `QA_CA_FILE`,
//!   `QA_INSECURE`, `QA_NODE` and `QA_NODE_IP` set.
//!
//! Output: `<out>/…` with `manifest.json` (every item and its status), then
//! `<out>.tar.gz` holding all of it, the manifest included (#13).

mod api;
mod cluster;
mod frame;
mod host;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use api::Api;

#[derive(Parser)]
#[command(name = "must-gather", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    gather: GatherArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// On a node, inside the collector pod: print the host's data as a
    /// framed tar.gz on stdout (must-gather starts this itself).
    HostCollect(host::CollectArgs),
}

#[derive(Args)]
struct GatherArgs {
    /// The apiserver, `https://<node>:6443`. Empty: in-cluster (from a pod).
    #[arg(long, env = "MUST_GATHER_API", default_value = "")]
    api: String,
    /// A file holding a bearer token (default: the pod's ServiceAccount).
    #[arg(long, env = "MUST_GATHER_TOKEN_FILE")]
    token_file: Option<String>,
    /// The cluster's CA (default: the pod's ServiceAccount CA).
    #[arg(long, env = "MUST_GATHER_CA_FILE")]
    ca_file: Option<String>,
    /// Skip verifying the apiserver's certificate.
    #[arg(long)]
    insecure: bool,
    /// Output directory; `<out>.tar.gz` is written next to it.
    /// Default: `must-gather-<unix time>` in the current directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Only these nodes (comma-separated names) for the host part; default all.
    #[arg(long, value_delimiter = ',')]
    nodes: Vec<String>,
    /// The image the per-node collector pod runs (it must hold must-gather
    /// at `--host-command`). Without it the host part is skipped.
    #[arg(long, env = "MUST_GATHER_HOST_IMAGE")]
    host_image: Option<String>,
    #[arg(long, default_value = "/must-gather")]
    host_command: String,
    /// Namespace for the collector pods. Default: a new
    /// `must-gather-<run>` namespace, deleted afterwards.
    #[arg(long)]
    host_namespace: Option<String>,
    /// How long a node's collector pod may take.
    #[arg(long, default_value_t = 300)]
    host_timeout: u64,
    /// Component collector scripts: `<dir>/<area>/*.sh`.
    #[arg(long)]
    collectors_dir: Option<PathBuf>,
    /// Per-script timeout (seconds).
    #[arg(long, default_value_t = 60)]
    timeout: u64,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Error,
    Skipped,
}

#[derive(Serialize, Debug)]
pub struct Item {
    pub path: String,
    pub status: Status,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Item {
    pub fn new(path: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Item { path: path.into(), status, detail: detail.into() }
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    must_gather: &'a str,
    run: &'a str,
    api: &'a str,
    started_unix: u64,
    nodes: Vec<String>,
    tarball: String,
    counts: Value,
    items: &'a [Item],
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Some(Cmd::HostCollect(a)) => host::host_collect(a).await,
        None => gather(cli.gather).await,
    };
    if let Err(e) = r {
        eprintln!("must-gather: {e:#}");
        std::process::exit(1);
    }
}

async fn gather(g: GatherArgs) -> anyhow::Result<()> {
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let run = format!("{:08x}", (started.as_nanos() as u64 ^ (std::process::id() as u64) << 32) as u32);
    let out = g.out.clone().unwrap_or_else(|| PathBuf::from(format!("must-gather-{}", started.as_secs())));
    std::fs::create_dir_all(&out)?;
    let api = Api::new(&g.api, g.token_file.as_deref(), g.ca_file.as_deref(), g.insecure)?;
    let mut items: Vec<Item> = Vec::new();

    eprintln!("must-gather: cluster state from {}", api.base);
    let nodes = cluster::gather(&api, &out, &mut items).await;
    let node_list: Vec<(String, String)> = nodes
        .as_ref()
        .map(api::items)
        .unwrap_or(&[])
        .iter()
        .filter_map(|n| {
            let name = n["metadata"]["name"].as_str()?.to_string();
            let ip = n["status"]["addresses"].as_array()?.iter().find(|a| a["type"] == "InternalIP")?["address"].as_str().unwrap_or("").to_string();
            Some((name, ip))
        })
        .filter(|(n, _)| g.nodes.is_empty() || g.nodes.contains(n))
        .collect();

    // The host part, per node.
    match &g.host_image {
        None => items.push(Item::new("nodes/*/host", Status::Skipped, "no --host-image: the node's logs, kernel log and stormblock state were not collected")),
        Some(image) if !node_list.is_empty() => {
            let (ns, made) = match &g.host_namespace {
                Some(ns) => (ns.clone(), false),
                None => {
                    let ns = format!("must-gather-{run}");
                    let r = api.send(reqwest::Method::POST, "/api/v1/namespaces", Some(&host::namespace_spec(&ns, &run))).await?;
                    if !r.ok() {
                        anyhow::bail!("creating namespace {ns} answered {}: {}", r.code, cluster::first_line(&r.text));
                    }
                    (ns, true)
                }
            };
            for (node, _) in &node_list {
                eprintln!("must-gather: host data from {node}");
                let path = format!("nodes/{node}/host");
                match host::gather(&api, &ns, node, image, &g.host_command, &run, Duration::from_secs(g.host_timeout), &out.join(&path)).await {
                    Ok(d) => items.push(Item::new(path, Status::Ok, d)),
                    Err(e) => items.push(Item::new(path, Status::Error, format!("{e:#}"))),
                }
            }
            if made {
                let _ = api.send(reqwest::Method::DELETE, &format!("/api/v1/namespaces/{ns}"), None).await;
            }
        }
        Some(_) => items.push(Item::new("nodes/*/host", Status::Skipped, "no nodes to collect from")),
    }

    // Component collector scripts, per node.
    for (area, script) in discover_collectors(g.collectors_dir.as_deref()) {
        for (node, ip) in &node_list {
            let name = script.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let path = format!("nodes/{node}/collectors/{area}/{name}.txt");
            let target = out.join(&path);
            let _ = std::fs::create_dir_all(target.parent().unwrap());
            let mut c = Command::new("sh");
            c.arg(&script)
                .env("QA_API", &api.base)
                .env("QA_TOKEN_FILE", api.token_file().unwrap_or(""))
                .env("QA_CA_FILE", g.ca_file.as_deref().unwrap_or(""))
                .env("QA_INSECURE", if g.insecure { "1" } else { "0" })
                .env("QA_NODE", node)
                .env("QA_NODE_IP", ip)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let (st, text) = run_capture(c, g.timeout).await;
            let _ = std::fs::write(&target, &text);
            items.push(Item::new(path, st, if st == Status::Ok { String::new() } else { cluster::first_line(&text) }));
        }
    }

    // The manifest goes in before the tarball is made (#13).
    let tarball = format!("{}.tar.gz", out.to_string_lossy().trim_end_matches(['/', '\\']));
    let count = |s: Status| items.iter().filter(|i| i.status == s).count();
    let manifest = Manifest {
        must_gather: env!("CARGO_PKG_VERSION"),
        run: &run,
        api: &api.base,
        started_unix: started.as_secs(),
        nodes: node_list.iter().map(|(n, _)| n.clone()).collect(),
        tarball: tarball.clone(),
        counts: serde_json::json!({"ok": count(Status::Ok), "error": count(Status::Error), "skipped": count(Status::Skipped)}),
        items: &items,
    };
    std::fs::write(out.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    write_tarball(&out, &tarball)?;
    println!(
        "must-gather: {} nodes, {} ok, {} errors, {} skipped (see manifest.json) -> {tarball}",
        node_list.len(),
        count(Status::Ok),
        count(Status::Error),
        count(Status::Skipped)
    );
    Ok(())
}

/// `<out>.tar.gz` holding `<out>/` under its own name (no `tar` program
/// needed: a Windows workstation has none).
fn write_tarball(out: &Path, tarball: &str) -> anyhow::Result<()> {
    let f = std::fs::File::create(tarball)?;
    let mut t = tar::Builder::new(flate2::write::GzEncoder::new(f, flate2::Compression::default()));
    let name = out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "must-gather".into());
    t.append_dir_all(&name, out)?;
    t.into_inner()?.finish()?;
    Ok(())
}

async fn run_capture(mut c: Command, timeout: u64) -> (Status, String) {
    match tokio::time::timeout(Duration::from_secs(timeout), c.output()).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr);
            if !e.trim().is_empty() {
                s.push_str("\n--- stderr ---\n");
                s.push_str(&e);
            }
            (if o.status.success() { Status::Ok } else { Status::Error }, s)
        }
        Ok(Err(e)) => (Status::Error, format!("(collector could not start: {e}; it needs `sh`)")),
        Err(_) => (Status::Error, "(collector timed out)".into()),
    }
}

/// `<dir>/<area>/*.sh`, sorted.
fn discover_collectors(dir: Option<&Path>) -> Vec<(String, PathBuf)> {
    let Some(dir) = dir else { return vec![] };
    let mut out = Vec::new();
    for a in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let ap = a.path();
        if !ap.is_dir() {
            continue;
        }
        let area = ap.file_name().unwrap_or_default().to_string_lossy().to_string();
        for f in std::fs::read_dir(&ap).into_iter().flatten().flatten() {
            let p = f.path();
            if p.is_file() && p.extension().is_some_and(|e| e == "sh") {
                out.push((area.clone(), p));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_inside_the_tarball() {
        let t = std::env::temp_dir().join(format!("mg-tar-{}", std::process::id()));
        let out = t.join("must-gather-1");
        std::fs::create_dir_all(out.join("cluster")).unwrap();
        std::fs::write(out.join("cluster/nodes.json"), "{}").unwrap();
        std::fs::write(out.join("manifest.json"), "{\"items\":[]}").unwrap();
        let tarball = format!("{}.tar.gz", out.display());
        write_tarball(&out, &tarball).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&tarball).unwrap()));
        let names: Vec<String> = ar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string()).collect();
        assert!(names.iter().any(|n| n == "must-gather-1/manifest.json"), "{names:?}");
        assert!(names.iter().any(|n| n == "must-gather-1/cluster/nodes.json"), "{names:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn collectors_are_sh_scripts_by_area() {
        let t = std::env::temp_dir().join(format!("mg-col-{}", std::process::id()));
        std::fs::create_dir_all(t.join("ironprom")).unwrap();
        std::fs::write(t.join("ironprom/status.sh"), "echo hi").unwrap();
        std::fs::write(t.join("ironprom/README"), "no").unwrap();
        assert_eq!(discover_collectors(Some(&t)), vec![("ironprom".to_string(), t.join("ironprom/status.sh"))]);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn plain_http_is_refused() {
        assert!(Api::new("http://127.0.0.1:6443", None, None, false).is_err());
        assert!(Api::new("https://127.0.0.1:6443", None, None, true).is_ok());
    }
}
