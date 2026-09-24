//! must-gather — Storm CoreOS debug-data collector (our `oc adm must-gather`).
//!
//! Fans out over SSH to one or more nodes and collects a structured snapshot:
//! kernel state, systemd + every storm component's journal/status, storage
//! (ublk/erofs/stormblock), network, and the cluster API — into
//! `<out>/<node>/<area>/<name>.txt`, tarred to `<out>.tar.gz`; `manifest.json`
//! is written into `<out>` after the tar, so it is not in the tarball (#13).
//! Built-ins assume a systemd node and run without sudo. Extensible: any
//! component can drop a collector script under `gather/<area>/`, run locally
//! per node with `QA_SSH`/`QA_NODE_IP` set (not `QA_API`), so components own
//! their own debug data the way they own their tests.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use clap::Parser;
use serde::Serialize;
use tokio::process::Command;

#[derive(Parser)]
#[command(name = "must-gather", version, about)]
struct Cli {
    /// Nodes to gather from (comma-separated ip or host).
    #[arg(long, value_delimiter = ',')]
    nodes: Vec<String>,
    /// SSH command template; `{node}` is substituted. The collector runs
    /// `<ssh> "<remote-cmd>"`.
    #[arg(long, default_value = "ssh -o StrictHostKeyChecking=no -o ConnectTimeout=8 root@{node}")]
    ssh: String,
    /// Output directory (a tarball is written alongside it).
    #[arg(long, default_value = "/tmp/must-gather")]
    out: PathBuf,
    /// Extra collector scripts dir (each gather/<area>/<name> is run per node).
    #[arg(long)]
    collectors_dir: Option<PathBuf>,
    /// Per-remote-command timeout (seconds).
    #[arg(long, default_value = "60")]
    timeout: u64,
}

/// (area, name, remote command).
type C = (&'static str, &'static str, &'static str);

/// Every storm component + kernel. Journals/status per component.
fn builtins() -> Vec<C> {
    let mut v: Vec<C> = vec![
        // kernel
        ("kernel", "uname", "uname -a"),
        ("kernel", "cmdline", "cat /proc/cmdline"),
        ("kernel", "dmesg", "dmesg | tail -n 800"),
        ("kernel", "modules", "lsmod"),
        ("kernel", "io_uring_disabled", "cat /proc/sys/kernel/io_uring_disabled 2>/dev/null"),
        ("kernel", "taint", "cat /proc/sys/kernel/tainted 2>/dev/null"),
        // system
        ("system", "os-release", "cat /etc/os-release /etc/stormcos-release 2>/dev/null"),
        ("system", "systemd-failed", "systemctl --failed --no-pager"),
        ("system", "systemd-running", "systemctl list-units --state=running --no-pager"),
        ("system", "boot-warnings", "journalctl -b -p warning -n 600 --no-pager"),
        ("system", "resources", "uptime; free -h; df -h"),
        // storage
        ("storage", "block", "lsblk; ls -l /dev/ublk* 2>/dev/null"),
        ("storage", "mounts", "findmnt; mount | grep -Ei 'erofs|overlay|ublk' || true"),
        // network
        ("network", "addr", "ip -br addr"),
        ("network", "route", "ip route"),
        ("network", "listen", "ss -tlnp 2>/dev/null || netstat -tlnp 2>/dev/null"),
        // cluster
        ("cluster", "nodes", "wget -qO- http://127.0.0.1:6443/api/v1/nodes 2>/dev/null"),
        ("cluster", "pods", "wget -qO- http://127.0.0.1:6443/api/v1/pods 2>/dev/null"),
        ("cluster", "events", "wget -qO- http://127.0.0.1:6443/api/v1/events 2>/dev/null"),
    ];
    // Per-component journal + status.
    for svc in [
        "kubelet",
        "kube-proxy",
        "kube-apiserver",
        "kube-controller-manager",
        "kube-scheduler",
        "fastetcd",
        "crio",
        "cadvisor",
        "ironprom",
        "stormblock",
        "stormblock-target",
        "sshd",
        "NetworkManager",
    ] {
        // Leak the small owned strings for 'static — fine for a one-shot tool.
        let jr: &'static str = Box::leak(
            format!("journalctl -u {svc} -n 500 --no-pager 2>/dev/null; systemctl status {svc} --no-pager 2>/dev/null")
                .into_boxed_str(),
        );
        let name: &'static str = Box::leak(svc.to_string().into_boxed_str());
        v.push(("components", name, jr));
    }
    v
}

#[derive(Serialize)]
struct Manifest {
    nodes: Vec<String>,
    collectors: usize,
    out: PathBuf,
    tarball: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(!cli.nodes.is_empty(), "--nodes required");
    std::fs::create_dir_all(&cli.out)?;
    let builtins = builtins();
    let extra = discover_collectors(cli.collectors_dir.as_deref());

    for node in &cli.nodes {
        let ssh = cli.ssh.replace("{node}", node);
        println!("== gathering {node} ==");
        for (area, name, cmd) in &builtins {
            let dir = cli.out.join(node).join(area);
            std::fs::create_dir_all(&dir)?;
            let out = remote(&ssh, cmd, cli.timeout).await;
            let _ = std::fs::write(dir.join(format!("{name}.txt")), out);
        }
        // Component-contributed collector scripts (run locally with QA_* env).
        for (area, path) in &extra {
            let dir = cli.out.join(node).join(area);
            std::fs::create_dir_all(&dir)?;
            let name = path.file_stem().unwrap().to_string_lossy();
            let out = run_script(path, node, &ssh, cli.timeout).await;
            let _ = std::fs::write(dir.join(format!("{name}.txt")), out);
        }
    }

    // Tar it up.
    let ts_dir = cli.out.file_name().unwrap().to_string_lossy().to_string();
    let tarball = format!("{}.tar.gz", cli.out.to_string_lossy());
    let _ = Command::new("tar")
        .args([
            "-czf",
            &tarball,
            "-C",
            &cli.out.parent().unwrap_or(Path::new(".")).to_string_lossy(),
            &ts_dir,
        ])
        .status()
        .await;
    let manifest = Manifest {
        nodes: cli.nodes.clone(),
        collectors: builtins.len() + extra.len(),
        out: cli.out.clone(),
        tarball: tarball.clone(),
    };
    std::fs::write(cli.out.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    println!(
        "\nmust-gather: {} nodes, {} collectors -> {}",
        cli.nodes.len(),
        manifest.collectors,
        tarball
    );
    Ok(())
}

async fn remote(ssh: &str, cmd: &str, timeout: u64) -> String {
    // ssh is a full command line; append the remote command as one arg.
    let mut parts = ssh.split_whitespace();
    let prog = parts.next().unwrap_or("ssh");
    let mut c = Command::new(prog);
    c.args(parts).arg(cmd).stdout(Stdio::piped()).stderr(Stdio::piped());
    run_capture(c, timeout).await
}

async fn run_script(path: &Path, node: &str, ssh: &str, timeout: u64) -> String {
    let mut c = Command::new(path);
    c.env("QA_NODE_IP", node)
        .env("QA_SSH", ssh)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_capture(c, timeout).await
}

async fn run_capture(mut c: Command, timeout: u64) -> String {
    match tokio::time::timeout(std::time::Duration::from_secs(timeout), c.output()).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr);
            if !e.trim().is_empty() {
                s.push_str("\n--- stderr ---\n");
                s.push_str(&e);
            }
            s
        }
        Ok(Err(e)) => format!("(collector error: {e})"),
        Err(_) => "(collector timed out)".into(),
    }
}

/// gather/<area>/<script> — component-owned collectors, executable.
fn discover_collectors(dir: Option<&Path>) -> Vec<(String, PathBuf)> {
    let Some(dir) = dir else { return vec![] };
    let mut out = Vec::new();
    let Ok(areas) = std::fs::read_dir(dir) else {
        return out;
    };
    for a in areas.flatten() {
        let ap = a.path();
        if !ap.is_dir() {
            continue;
        }
        let area = ap.file_name().unwrap().to_string_lossy().to_string();
        if let Ok(files) = std::fs::read_dir(&ap) {
            for f in files.flatten() {
                let p = f.path();
                if p.is_file() && is_exec(&p) {
                    out.push((area.clone(), p));
                }
            }
        }
    }
    out
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
