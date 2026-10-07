//! What only the node has: stormpump's logs and asset status, the stormd
//! service logs, pod log files, the kernel log, pstore, host network and
//! mounts, stormblock's engine API and fastetcd's metrics.
//!
//! A node has no ssh for this (and ssh lands in the `fedora` container, not
//! on the host), so must-gather starts one short-lived pod per node, pinned
//! with `spec.nodeName`, with `hostPID`, `hostNetwork` and read-only host
//! mounts under `/host`, running `must-gather host-collect`. That prints a
//! tar.gz as a [frame](crate::frame) on its log, which the workstation reads
//! through `pods/log` and unpacks under `nodes/<node>/host/`.
//!
//! Secrets stay on the node: files named like a key, token or secret, and
//! everything under `/state/config` but `stormcos.toml`, are listed with
//! their size, never copied; config files that are copied have the values
//! of secret-looking keys redacted.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::api::{Api, enc};
use crate::frame;

/// Host paths the pod mounts read-only, each at `/host<path>`. At most 16
/// (rustkube-node drops mounts past 16 silently).
pub const MOUNTS: [&str; 8] = [
    "/run/stormpump",
    "/l",
    "/var/log/pods",
    "/dev",
    "/sys/fs/pstore",
    "/etc/stormpump",
    "/state/config",
    "/run/stormblock/engine/api_token",
];

/// The collector pod for `node`.
pub fn pod_spec(name: &str, node: &str, image: &str, command: &str, run: &str) -> Value {
    let volumes: Vec<Value> = MOUNTS
        .iter()
        .enumerate()
        .map(|(i, p)| json!({"name": format!("host-{i}"), "hostPath": {"path": p}}))
        .collect();
    let mounts: Vec<Value> = MOUNTS
        .iter()
        .enumerate()
        .map(|(i, p)| json!({"name": format!("host-{i}"), "mountPath": format!("/host{p}"), "readOnly": true}))
        .collect();
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "labels": {"storm.io/purpose": "must-gather", "storm.io/must-gather": run}},
        "spec": {
            "nodeName": node,
            "restartPolicy": "Never",
            "hostPID": true,
            "hostNetwork": true,
            "automountServiceAccountToken": false,
            "terminationGracePeriodSeconds": 1,
            "tolerations": [{"operator": "Exists"}],
            "containers": [{
                "name": "gather",
                "image": image,
                "imagePullPolicy": "IfNotPresent",
                "command": [command],
                "args": ["host-collect"],
                "securityContext": {"privileged": true},
                "volumeMounts": mounts,
            }],
            "volumes": volumes,
        }
    })
}

/// A DNS-label pod name for `node` in run `run`.
pub fn pod_name(node: &str, run: &str) -> String {
    let n: String = node.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut name = format!("must-gather-{}-{run}", n.trim_matches('-'));
    name.truncate(63);
    name.trim_end_matches('-').to_string()
}

/// Run the collector pod on `node` in `ns` and unpack what it sends into
/// `dir`. Always deletes the pod. Returns a one-line status.
pub async fn gather(api: &Api, ns: &str, node: &str, image: &str, command: &str, run: &str, timeout: Duration, dir: &Path) -> Result<String> {
    let name = pod_name(node, run);
    let pods = format!("/api/v1/namespaces/{ns}/pods");
    let _ = api.send(reqwest::Method::DELETE, &format!("{pods}/{name}"), None).await;
    let r = api.send(reqwest::Method::POST, &pods, Some(&pod_spec(&name, node, image, command, run))).await?;
    if !r.ok() {
        anyhow::bail!("creating pod {ns}/{name} answered {}: {}", r.code, short(&r.text));
    }
    let out = collect(api, &pods, &name, timeout, dir).await;
    let _ = api.send(reqwest::Method::DELETE, &format!("{pods}/{name}"), None).await;
    out
}

async fn collect(api: &Api, pods: &str, name: &str, timeout: Duration, dir: &Path) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    loop {
        let g = api.get(&format!("{pods}/{name}")).await?;
        let p: Value = serde_json::from_str(&g.text).unwrap_or(Value::Null);
        let phase = p["status"]["phase"].as_str().unwrap_or("").to_string();
        if phase == "Succeeded" || phase == "Failed" {
            last = phase;
            break;
        }
        let waiting = p["status"]["containerStatuses"][0]["state"]["waiting"]["reason"].as_str().unwrap_or("");
        last = format!("{phase} {waiting}").trim().to_string();
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("pod {name} did not finish in {}s (last: {last})", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let log = api.get(&format!("{pods}/{name}/log?container=gather")).await?;
    if !log.ok() {
        anyhow::bail!("reading pod {name}'s log answered {}: {}", log.code, short(&log.text));
    }
    let tgz = frame::decode(&log.text).map_err(|e| anyhow::anyhow!("pod {name} ({last}): {e}"))?;
    std::fs::create_dir_all(dir)?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&tgz[..]));
    let mut files = 0;
    for e in ar.entries()? {
        let mut e = e?;
        if e.unpack_in(dir)? {
            files += 1;
        }
    }
    Ok(format!("{files} files, {} bytes compressed", tgz.len()))
}

fn short(s: &str) -> String {
    s.chars().take(300).collect()
}

// ---- on the node: `must-gather host-collect` ----

#[derive(clap::Args, Debug)]
pub struct CollectArgs {
    /// Where the host's paths are mounted.
    #[arg(long, default_value = "/host")]
    pub root: PathBuf,
    /// The host's /proc (the pod has hostPID, so its own /proc is the host's).
    #[arg(long, default_value = "/proc")]
    pub proc_root: PathBuf,
    /// stormblock's engine API (the pod has hostNetwork).
    #[arg(long, default_value = "http://127.0.0.1:9090")]
    pub stormblock: String,
    /// fastetcd's plain metrics port (its client port is mutual TLS).
    #[arg(long, default_value = "http://127.0.0.1:2381/metrics")]
    pub fastetcd_metrics: String,
    /// The most uncompressed bytes to put in the bundle; logs past it are
    /// named in errors.txt, not copied.
    #[arg(long, default_value_t = 512 << 20)]
    pub max_bytes: u64,
}

/// Collect into a tar.gz and print it framed on stdout.
pub async fn host_collect(a: CollectArgs) -> Result<()> {
    let tgz = bundle(&a).await?;
    let mut out = std::io::stdout().lock();
    out.write_all(frame::encode(&tgz).as_bytes())?;
    out.flush()?;
    Ok(())
}

struct Bundle {
    tar: tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    bytes: u64,
    max: u64,
    errors: Vec<String>,
}

impl Bundle {
    fn add(&mut self, name: &str, data: &[u8]) {
        if self.bytes + data.len() as u64 > self.max {
            self.errors.push(format!("{name}: not copied, the bundle is at its {} byte limit", self.max));
            return;
        }
        self.bytes += data.len() as u64;
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
        h.set_cksum();
        // ':' is not a file name character on a Windows workstation.
        let name = name.replace(':', "_");
        if let Err(e) = self.tar.append_data(&mut h, &name, data) {
            self.errors.push(format!("{name}: {e}"));
        }
    }

    fn file(&mut self, name: &str, path: &Path, tail: u64) {
        match read_tail(path, tail) {
            Ok(b) => self.add(name, &b),
            Err(e) => self.errors.push(format!("{}: {e}", path.display())),
        }
    }

    /// Every file under `dir` (recursively), each to its last `tail` bytes,
    /// with secret-looking ones listed instead.
    fn tree(&mut self, prefix: &str, dir: &Path, tail: u64) {
        let mut listing = String::new();
        for (rel, path, len) in walk(dir) {
            listing.push_str(&format!("{len}\t{rel}\n"));
            if secret_name(&rel) {
                continue;
            }
            self.file(&format!("{prefix}/{rel}"), &path, tail);
        }
        if listing.is_empty() && !dir.exists() {
            self.errors.push(format!("{}: not mounted or absent", dir.display()));
        }
        self.add(&format!("{prefix}.listing.txt"), listing.as_bytes());
    }
}

/// Tail of a file: the last `tail` bytes (0 = all).
fn read_tail(path: &Path, tail: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if tail > 0 && len > tail {
        f.seek(SeekFrom::Start(len - tail))?;
    }
    let mut b = Vec::new();
    f.take(if tail > 0 { tail } else { u64::MAX }).read_to_end(&mut b)?;
    Ok(b)
}

/// Regular files under `dir`: (relative path, path, length), sorted.
fn walk(dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                out.push((rel, p, m.len()));
            }
        }
    }
    out.sort();
    out
}

/// A file that may hold a secret: listed, never copied.
pub fn secret_name(rel: &str) -> bool {
    let l = rel.to_lowercase();
    ["token", "secret", "password", "passwd", "credential", "private", "pull-secret", "authorized_keys", "shadow", ".pem", ".key", "id_rsa", "id_ed25519", "kubeconfig"]
        .iter()
        .any(|w| l.contains(w))
}

/// Config text with the values of secret-looking keys replaced.
pub fn redact(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let key = line.split(['=', ':']).next().unwrap_or("").to_lowercase();
        let secret = ["token", "secret", "password", "passwd", "credential", "apikey", "api_key", "private"].iter().any(|w| key.contains(w));
        match line.find(['=', ':']) {
            Some(i) if secret => {
                out.push_str(&line[..=i]);
                out.push_str(" <redacted>");
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

async fn bundle(a: &CollectArgs) -> Result<Vec<u8>> {
    let mut b = Bundle { tar: tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default())), bytes: 0, max: a.max_bytes, errors: Vec::new() };
    let host = |p: &str| a.root.join(p.trim_start_matches('/'));
    let pr = |p: &str| a.proc_root.join(p);

    // Kernel and host state (hostPID: /proc is the host's; hostNetwork: so is /proc/net).
    for f in ["version", "cmdline", "uptime", "loadavg", "meminfo", "modules", "partitions", "stat", "1/mountinfo", "sys/kernel/tainted", "sys/kernel/io_uring_disabled", "pressure/cpu", "pressure/memory", "pressure/io"] {
        b.file(&format!("proc/{f}"), &pr(f), 0);
    }
    for f in ["dev", "route", "ipv6_route", "if_inet6", "tcp", "tcp6", "udp", "udp6", "arp", "snmp", "netstat", "unix"] {
        b.file(&format!("net/{f}"), &pr(&format!("net/{f}")), 4 << 20);
    }
    b.add("kernel/kmsg.txt", &kmsg(&host("/dev/kmsg")).unwrap_or_else(|e| format!("(kmsg: {e})\n").into_bytes()));
    let mut devs: Vec<String> = std::fs::read_dir(host("/dev")).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    devs.sort();
    b.add("kernel/dev.listing.txt", (devs.join("\n") + "\n").as_bytes());
    for d in ["/sys/class/ublk-char", "/sys/block", "/sys/class/net", "/sys/class/nvme"] {
        let mut names: Vec<String> = std::fs::read_dir(d).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        b.add(&format!("kernel/{}.listing.txt", d.trim_start_matches('/').replace('/', "_")), (names.join("\n") + "\n").as_bytes());
    }
    b.tree("kernel/pstore", &host("/sys/fs/pstore"), 0);

    // stormpump: PID 1, the node's service supervisor.
    b.file("stormpump/assets.json", &host("/run/stormpump/assets.json"), 0);
    b.file("stormpump/runs.tsv", &host("/run/stormpump/runs.tsv"), 8 << 20);
    b.tree("stormpump/logs", &host("/run/stormpump/logs"), 4 << 20);
    // stormd's own rotated service logs (/logs -> /l on the host).
    b.tree("services", &host("/l"), 4 << 20);
    // Every container's log as written, before the kubelet's /log filter (#136).
    b.tree("pods", &host("/var/log/pods"), 1 << 20);

    // Configuration: unit files redacted; /state/config listed, only the manifest copied.
    for (rel, path, _) in walk(&host("/etc/stormpump")) {
        if secret_name(&rel) {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(t) => b.add(&format!("config/etc-stormpump/{rel}"), redact(&t).as_bytes()),
            Err(e) => b.errors.push(format!("{}: {e}", path.display())),
        }
    }
    let state = host("/state/config");
    let listing: String = walk(&state).iter().map(|(r, _, l)| format!("{l}\t{r}\n")).collect();
    b.add("config/state-config.listing.txt", listing.as_bytes());
    match std::fs::read_to_string(state.join("stormcos.toml")) {
        Ok(t) => b.add("config/stormcos.toml", redact(&t).as_bytes()),
        Err(e) => b.errors.push(format!("{}/stormcos.toml: {e}", state.display())),
    }

    // stormblock's engine (reads need the node token; never copied).
    let token = std::fs::read_to_string(host("/run/stormblock/engine/api_token")).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if token.is_none() {
        b.errors.push("stormblock: no engine token at /run/stormblock/engine/api_token; only its open endpoints".into());
    }
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    for (name, path) in [
        ("health", "/api/v1/health"),
        ("volumes", "/api/v1/volumes"),
        ("slabs", "/api/v1/slabs"),
        ("slabs-pool", "/api/v1/slabs/pool"),
        ("slabs-durability", "/api/v1/slabs/durability"),
        ("drives", "/api/v1/drives"),
        ("arrays", "/api/v1/arrays"),
        ("pallets-status", "/api/v1/pallets/status"),
        ("exports", "/api/v1/exports"),
        ("rebuilds", "/api/v1/rebuilds"),
        ("releases", "/api/v1/releases"),
        ("debug-stalls", "/debug/stalls"),
        ("debug-tasks", "/debug/tasks"),
        ("metrics", "/metrics"),
    ] {
        let mut req = http.get(format!("{}{path}", a.stormblock.trim_end_matches('/')));
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        let ext = if name == "metrics" || name.starts_with("debug") { "txt" } else { "json" };
        match req.send().await {
            Ok(r) => {
                let code = r.status();
                let body = r.bytes().await.unwrap_or_default();
                if !code.is_success() {
                    b.errors.push(format!("stormblock {path}: {code}"));
                }
                b.add(&format!("stormblock/{name}.{ext}"), &body);
            }
            Err(e) => b.errors.push(format!("stormblock {path}: {e}")),
        }
    }
    match http.get(&a.fastetcd_metrics).send().await {
        Ok(r) => {
            let body = r.bytes().await.unwrap_or_default();
            b.add("fastetcd/metrics.txt", &body);
        }
        Err(e) => b.errors.push(format!("fastetcd metrics {}: {e}", a.fastetcd_metrics)),
    }

    let errors = b.errors.join("\n") + "\n";
    b.add("errors.txt", errors.as_bytes());
    let gz = b.tar.into_inner().context("finishing the tar")?;
    Ok(gz.finish()?)
}

/// The kernel's log buffer, read from /dev/kmsg without waiting for new
/// records (stormpump only forwards it to syslog; no file holds it). Never
/// /proc/kmsg: reading that consumes the messages.
#[cfg(unix)]
fn kmsg(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    const O_NONBLOCK: i32 = 0o4000; // Linux, every architecture stormcos runs on
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(path)?;
    let mut out = Vec::new();
    let mut rec = vec![0u8; 8192];
    loop {
        match f.read(&mut rec) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&rec[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            // A record overwritten while we read it: skip it and go on.
            Err(e) if e.raw_os_error() == Some(32) => continue,
            Err(e) if out.is_empty() => return Err(e),
            Err(_) => break,
        }
        if out.len() > 16 << 20 {
            break;
        }
    }
    Ok(out)
}

#[cfg(not(unix))]
fn kmsg(_: &Path) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("host-collect runs on a stormcos node"))
}

/// The namespace selector a created namespace gets: no PodSecurity
/// enforcement, so the host pod is admitted.
pub fn namespace_spec(name: &str, run: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name, "labels": {
        "storm.io/purpose": "must-gather", "storm.io/must-gather": run,
        "pod-security.kubernetes.io/enforce": "privileged"}}})
}

pub fn log_path(ns: &str, pod: &str, container: &str, previous: bool) -> String {
    format!(
        "/api/v1/namespaces/{ns}/pods/{pod}/log?container={}&tailLines=5000&limitBytes=4194304{}",
        enc(container),
        if previous { "&previous=true" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pod_is_pinned_privileged_and_read_only() {
        let p = pod_spec("must-gather-n1-r1", "n1", "img:1", "/must-gather", "r1");
        assert_eq!(p["spec"]["nodeName"], "n1");
        assert_eq!(p["spec"]["hostPID"], true);
        assert_eq!(p["spec"]["hostNetwork"], true);
        let mounts = p["spec"]["containers"][0]["volumeMounts"].as_array().unwrap();
        assert!(mounts.len() <= 16);
        assert!(mounts.iter().all(|m| m["readOnly"] == true && m["mountPath"].as_str().unwrap().starts_with("/host/")));
        assert_eq!(p["spec"]["containers"][0]["args"][0], "host-collect");
    }

    #[test]
    fn pod_names_are_dns_labels() {
        assert_eq!(pod_name("storm-06f96d", "a1b2"), "must-gather-storm-06f96d-a1b2");
        let n = pod_name(&"Node.Example_".repeat(10), "r");
        assert!(n.len() <= 63 && !n.ends_with('-') && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{n}");
    }

    #[test]
    fn secrets_are_listed_not_copied_and_redacted() {
        for s in ["pull-secret.json", "token-auth.csv", "ssh/authorized_keys", "engine/api_token", "tls/node.key"] {
            assert!(secret_name(s), "{s}");
        }
        assert!(!secret_name("fastetcd/fastetcd.log"));
        let r = redact("name = \"n1\"\nadmin_token = \"abc\"\npassword: hunter2\nport = 9090\n");
        assert!(r.contains("name = \"n1\"") && r.contains("port = 9090"));
        assert!(!r.contains("abc") && !r.contains("hunter2"), "{r}");
    }

    #[tokio::test]
    async fn a_bundle_from_a_fake_host_unpacks_and_keeps_secrets_out() {
        let t = std::env::temp_dir().join(format!("mg-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("host");
        let proc_root = t.join("proc");
        for (p, body) in [
            ("host/run/stormpump/assets.json", "[{\"name\":\"fastetcd\",\"running\":true}]"),
            ("host/run/stormpump/logs/w1.log", "a log line with spaces in it\n"),
            ("host/l/fastetcd/fastetcd.log", "2026-10-07T12:00:00Z stdout info started\n"),
            ("host/state/config/stormcos.toml", "node = \"n1\"\njoin_token = \"s3cret\"\n"),
            ("host/state/config/pull-secret.json", "{\"auths\":{}}"),
            ("host/run/stormblock/engine/api_token", "tok"),
            ("proc/version", "Linux version 7.2\n"),
        ] {
            let f = t.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        let a = CollectArgs { root, proc_root, stormblock: "http://127.0.0.1:9".into(), fastetcd_metrics: "http://127.0.0.1:9/metrics".into(), max_bytes: 1 << 20 };
        let tgz = bundle(&a).await.unwrap();
        // Through the frame and back, as the workstation sees it.
        let back = frame::decode(&frame::encode(&tgz)).unwrap();
        let out = t.join("out");
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&back[..]));
        ar.unpack(&out).unwrap();
        let read = |p: &str| std::fs::read_to_string(out.join(p)).unwrap_or_default();
        assert!(read("stormpump/assets.json").contains("fastetcd"));
        assert!(read("stormpump/logs/w1.log").contains("a log line with spaces"));
        assert!(read("services/fastetcd/fastetcd.log").contains("started"));
        assert!(read("proc/version").contains("Linux"));
        assert!(read("config/stormcos.toml").contains("node = \"n1\""));
        assert!(!read("config/stormcos.toml").contains("s3cret"));
        assert!(read("config/state-config.listing.txt").contains("pull-secret.json"));
        assert!(!out.join("config/pull-secret.json").exists());
        assert!(read("errors.txt").contains("stormblock"));
        let all: String = walk(&out).iter().map(|(_, p, _)| std::fs::read_to_string(p).unwrap_or_default()).collect();
        assert!(!all.contains("tok\n") && !all.contains("s3cret"));
        let _ = std::fs::remove_dir_all(&t);
    }
}
