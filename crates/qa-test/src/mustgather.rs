//! `/test must-gather` (#45): must-gather, run the way a pod runs it, against
//! the machine it is on, and what it brought back checked.
//!
//! The image carries `/must-gather` next to `/test`, so the per-node
//! collector pod runs from this same image, in the run namespace. The run's
//! cluster reads are what `[must-gather]` declares; the runner grants no
//! subresources, so `pods/log` outside the run namespace (kube-system's
//! service logs) answers 403 here and is not counted as a failure: the host
//! bundle carries those logs from the node's files.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::Value;

use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};

#[derive(Parser, Debug)]
#[command(name = "test must-gather", about = "must-gather against this machine, and its bundle checked")]
pub struct Args {
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    #[arg(long, env = "STORM_RUN_ID", default_value = "manual")]
    run_id: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: PathBuf,
    /// The must-gather binary (in this image).
    #[arg(long, default_value = "/must-gather")]
    bin: String,
    /// The image the collector pods run (default: this pod's own).
    #[arg(long)]
    image: Option<String>,
    /// How long must-gather may take in all.
    #[arg(long, default_value_t = 600)]
    timeout: u64,
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "must-gather");
    let ns = a.namespace.clone().unwrap_or_else(|| {
        std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace").unwrap_or_else(|_| "default".into()).trim().to_string()
    });
    let t = Instant::now();
    let kube = match Client::new("", None, false).await {
        Ok(k) => k,
        Err(e) => {
            out.emit(Line::new("must-gather/run", Status::Fail, t.elapsed(), format!("could not run: {e:#}")));
            out.summary();
            return 2;
        }
    };
    let image = match a.image.clone() {
        Some(i) => i,
        None => match own_image(&kube, &ns, &a.run_id).await {
            Some(i) => i,
            None => {
                out.emit(Line::new("must-gather/run", Status::Fail, t.elapsed(), format!("could not run: this pod's image not found in {ns} (label storm.io/test-run={}); pass --image", a.run_id)));
                out.summary();
                return 2;
            }
        },
    };

    // Run it.
    let dir = a.results.join("must-gather");
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = tokio::process::Command::new(&a.bin);
    c.args(["--out", &dir.to_string_lossy(), "--host-image", &image, "--host-namespace", &ns, "--host-timeout", "240"]);
    let run = tokio::time::timeout(Duration::from_secs(a.timeout), c.output()).await;
    let (ok, said) = match run {
        Ok(Ok(o)) => (o.status.success(), format!("{} {}", String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))),
        Ok(Err(e)) => (false, format!("{}: {e}", a.bin)),
        Err(_) => (false, format!("did not finish in {}s", a.timeout)),
    };
    let tarball = PathBuf::from(format!("{}.tar.gz", dir.display()));
    let ran = ok && tarball.exists();
    out.emit(Line::new("must-gather/run", if ran { Status::Pass } else { Status::Fail }, t.elapsed(), said.trim()));
    if !ran {
        out.summary();
        return 1;
    }

    let manifest: Value = std::fs::read_to_string(dir.join("manifest.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    for (test, (st, detail)) in checks(&dir, &manifest) {
        out.emit(Line::new(test, st, t.elapsed(), detail));
    }

    // Nothing of its own left behind.
    let left = kube.get(&format!("/api/v1/namespaces/{ns}/pods?labelSelector=storm.io%2Fpurpose%3Dmust-gather")).await;
    let (st, d) = match left {
        Ok(r) if r.ok() && kube::items(&r.body).is_empty() => (Status::Pass, "no collector pod left".to_string()),
        Ok(r) if r.ok() => (Status::Fail, format!("{} collector pod(s) left in {ns}", kube::items(&r.body).len())),
        Ok(r) => (Status::Fail, format!("listing pods in {ns} answered {}", r.code)),
        Err(e) => (Status::Fail, format!("{e:#}")),
    };
    out.emit(Line::new("must-gather/cleanup", st, t.elapsed(), d));
    out.summary();
    if out.failed() > 0 { 1 } else { 0 }
}

/// This run's own pod (the runner labels it `storm.io/test-run`), else the
/// hostname match.
async fn own_image(kube: &Client, ns: &str, run: &str) -> Option<String> {
    let r = kube.get(&format!("/api/v1/namespaces/{ns}/pods?labelSelector=storm.io%2Ftest-run%3D{run}")).await.ok()?;
    let pods = kube::items(&r.body);
    let mine = pods.iter().filter(|p| p["metadata"]["labels"]["storm.io/purpose"] != "must-gather").find_map(|p| p["spec"]["containers"][0]["image"].as_str().map(str::to_string));
    mine.or_else(|| crate::long::own_image(&pods, &std::env::var("HOSTNAME").unwrap_or_default()))
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// The bundle's checks, from the unpacked output and its manifest.
pub fn checks(dir: &Path, manifest: &Value) -> Vec<(&'static str, (Status, String))> {
    let mut v = Vec::new();
    let items = manifest["items"].as_array().cloned().unwrap_or_default();
    let item = |p: &str| items.iter().find(|i| i["path"] == p).cloned().unwrap_or(Value::Null);

    // The manifest lists every item (that it is inside the tarball, #13, is
    // must-gather's own unit test).
    v.push((
        "must-gather/manifest",
        if !items.is_empty() {
            (Status::Pass, format!("{} items: {}", items.len(), manifest["counts"]))
        } else {
            (Status::Fail, "manifest.json missing or empty".into())
        },
    ));

    // Cluster state through the API.
    let nodes: Value = serde_json::from_str(&read(&dir.join("cluster/nodes.json"))).unwrap_or(Value::Null);
    let n_nodes = nodes["items"].as_array().map_or(0, Vec::len);
    let pods_ok = item("cluster/pods.json")["status"] == "ok";
    let errors: Vec<String> = items
        .iter()
        .filter(|i| i["status"] == "error" && i["path"].as_str().is_some_and(|p| p.starts_with("cluster/")))
        .filter_map(|i| i["path"].as_str().map(|p| p.trim_start_matches("cluster/").to_string()))
        .collect();
    v.push((
        "must-gather/cluster",
        if n_nodes > 0 && pods_ok {
            (Status::Pass, format!("{n_nodes} node(s), pods listed; not granted here: [{}]", errors.join(",")))
        } else {
            (Status::Fail, format!("nodes {n_nodes}, pods listed {pods_ok}; errors [{}]", errors.join(",")))
        },
    ));

    // Each node's host bundle.
    for n in nodes["items"].as_array().cloned().unwrap_or_default() {
        let name = n["metadata"]["name"].as_str().unwrap_or("").to_string();
        let h = dir.join("nodes").join(&name).join("host");
        let mut missing = Vec::new();
        for f in ["stormpump/assets.json", "proc/version", "proc/1/mountinfo", "kernel/kmsg.txt", "net/dev", "stormblock/volumes.json", "cgroup/tree.txt", "services.listing.txt", "pods.listing.txt"] {
            if std::fs::metadata(h.join(f)).map_or(true, |m| m.len() == 0) {
                missing.push(f);
            }
        }
        let volumes: Value = serde_json::from_str(&read(&h.join("stormblock/volumes.json"))).unwrap_or(Value::Null);
        if volumes["items"].as_array().is_none() {
            missing.push("stormblock/volumes.json is not a volume list");
        }
        let service_logs = std::fs::read_dir(h.join("services")).map(|r| r.count()).unwrap_or(0);
        let errs = read(&h.join("errors.txt"));
        let errs: Vec<&str> = errs.lines().filter(|l| !l.is_empty()).collect();
        v.push((
            "must-gather/host",
            if missing.is_empty() && service_logs > 0 {
                (Status::Pass, format!("{name}: {} ({service_logs} service log dirs; host errors: {})", item(&format!("nodes/{name}/host"))["detail"].as_str().unwrap_or(""), errs.join(" | ")))
            } else {
                (Status::Fail, format!("{name}: missing or empty [{}], {service_logs} service log dirs; {}; host errors: {}", missing.join(","), item(&format!("nodes/{name}/host"))["detail"].as_str().unwrap_or(""), errs.join(" | ")))
            },
        ));
    }

    // Secrets stay on the node.
    let copied: Vec<String> = walk(dir).into_iter().filter(|p| {
        let l = p.to_lowercase();
        !l.ends_with(".listing.txt") && ["pull-secret", "token-auth", "authorized_keys", "api_token", "admin_token", ".key", ".pem"].iter().any(|w| l.contains(w))
    }).collect();
    v.push((
        "must-gather/no-secrets",
        if copied.is_empty() { (Status::Pass, "no key, token or pull secret copied".into()) } else { (Status::Fail, format!("copied: {}", copied.join(","))) },
    ));
    v
}

fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn put(dir: &Path, p: &str, body: &str) {
        let f = dir.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
    }

    #[test]
    fn a_full_bundle_passes_and_a_copied_secret_fails() {
        let dir = std::env::temp_dir().join(format!("mg-suite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        put(&dir, "cluster/nodes.json", r#"{"items":[{"metadata":{"name":"n1"}}]}"#);
        let h = "nodes/n1/host/";
        for f in ["stormpump/assets.json", "proc/version", "proc/1/mountinfo", "kernel/kmsg.txt", "net/dev", "cgroup/tree.txt", "services.listing.txt", "pods.listing.txt", "services/fastetcd/fastetcd.log"] {
            put(&dir, &format!("{h}{f}"), "x\n");
        }
        put(&dir, &format!("{h}stormblock/volumes.json"), r#"{"items":[],"count":0}"#);
        put(&dir, &format!("{h}config/state-config.listing.txt"), "12\tpull-secret.json\n");
        let manifest = json!({"counts": {"ok": 3}, "items": [
            {"path": "cluster/nodes.json", "status": "ok"}, {"path": "cluster/pods.json", "status": "ok"},
            {"path": "cluster/leases.json", "status": "error", "detail": "403"},
            {"path": "nodes/n1/host", "status": "ok", "detail": "40 files"}]});
        let got = checks(&dir, &manifest);
        assert!(got.iter().all(|(_, (st, _))| *st == Status::Pass), "{got:?}");
        assert!(got.iter().any(|(t, (_, d))| *t == "must-gather/cluster" && d.contains("leases.json")), "{got:?}");

        put(&dir, &format!("{h}config/pull-secret.json"), "{}");
        std::fs::remove_file(dir.join(format!("{h}kernel/kmsg.txt"))).unwrap();
        let got = checks(&dir, &manifest);
        let st = |t: &str| got.iter().find(|(n, _)| *n == t).map(|(_, (s, d))| (*s, d.clone())).unwrap();
        assert_eq!(st("must-gather/no-secrets").0, Status::Fail);
        assert!(st("must-gather/host").1.contains("kernel/kmsg.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
