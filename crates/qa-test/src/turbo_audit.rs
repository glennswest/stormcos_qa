//! `/test turbomode`'s storage audit (#26): a read-only inventory of what
//! the run's claims and Pods hold on the node, taken in-process at three
//! points — `before` the run, `allocated` (all claims bound, every Pod done)
//! and `after` cleanup. API objects disappearing is not proof that storage
//! was reclaimed; this is.
//!
//! Owner's decision (#26, 2026-09-29): it runs on the node, in the test Job,
//! with no ssh. The Job's host access is read-only: `hostPID`, hostPath
//! `/proc`, `/sys/fs/cgroup`, `/proc/1/mountinfo` and only the file
//! `/run/stormblock/engine/api_token`. It reads:
//!
//! - stormblock's API (with the engine token): the run's volumes, by the
//!   built-in driver's `pvc-<ns>-<claim>` name or the PVs' volume handles,
//!   and every slab slot allocated to them;
//! - every host process's mountinfo and cgroup lines naming a run Pod's UID,
//!   a volume id or name, and the host init's mountinfo;
//! - cgroup directories naming them (the kubelet writes a Pod UID with
//!   underscores in systemd slice names, so both forms count);
//! - per-Pod kubelet directories, only when a kubelet pods dir is mounted
//!   (it is not in the owner's list): otherwise recorded `unmeasured`.
//!
//! One Job sees one node, so the driver fails the audit unless the cluster
//! has exactly that node. A port of `tools/turbomode/stormblock-audit.py`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Before,
    Allocated,
    After,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Before => "before",
            Phase::Allocated => "allocated",
            Phase::After => "after",
        }
    }
}

/// Where the host's state is mounted, and how to reach stormblock.
#[derive(Debug, Clone)]
pub struct Host {
    pub proc_root: PathBuf,
    pub host_mountinfo: PathBuf,
    pub cgroup_root: PathBuf,
    pub token_file: PathBuf,
    pub stormblock: String,
    pub kubelet_pods: Option<PathBuf>,
}

impl Host {
    /// Why this Job cannot audit the host, if it cannot: the mounts the
    /// owner's Job spec gives are missing (stormcentral#74 not in place).
    pub fn unusable(&self) -> Option<String> {
        if !self.proc_root.join("1").is_dir() {
            return Some(format!("{} shows no pid 1: the /proc mount is missing", self.proc_root.display()));
        }
        // Without hostPID this process is pid 1 of its own namespace.
        if self.proc_root == Path::new("/proc") && std::process::id() == 1 {
            return Some("not hostPID: this test is pid 1, so /proc is the container's".into());
        }
        if !self.host_mountinfo.is_file() {
            return Some(format!("host mountinfo {} is not mounted", self.host_mountinfo.display()));
        }
        if !self.cgroup_root.is_dir() {
            return Some(format!("cgroup tree {} is not mounted", self.cgroup_root.display()));
        }
        if let Err(e) = std::fs::read_to_string(&self.token_file) {
            return Some(format!("stormblock token {}: {e}", self.token_file.display()));
        }
        None
    }
}

/// What to look for.
#[derive(Debug, Default, Clone)]
pub struct Request {
    /// Volume names: `pvc-<ns>-<claim>` (the built-in driver's).
    pub names: Vec<String>,
    /// Pod UIDs.
    pub uids: Vec<String>,
    /// Volume ids known already (PV handles, the allocated audit's).
    pub ids: Vec<String>,
}

async fn stormblock_items(http: &reqwest::Client, base: &str, token: &str, path: &str) -> Result<Vec<Value>> {
    let url = format!("{}/api/v1/{path}", base.trim_end_matches('/'));
    let r = http.get(&url).bearer_auth(token).send().await.with_context(|| format!("GET {url}"))?;
    let code = r.status();
    let v: Value = r.json().await.with_context(|| format!("GET {url}: not JSON"))?;
    if !code.is_success() {
        bail!("GET {url} answered {code}: {v}");
    }
    let items = v["items"].as_array().cloned().with_context(|| format!("GET {url}: no items"))?;
    if v["count"].as_u64().is_some_and(|c| c as usize != items.len()) {
        bail!("incomplete storage inventory at {path}: count {} but {} items", v["count"], items.len());
    }
    Ok(items)
}

/// Lines of `path` that contain any needle, tagged with `pid`. An unreadable
/// file (the process exited) contributes nothing.
fn matching_lines(path: &Path, needles: &[String], pid: &str, out: &mut Vec<Value>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        if needles.iter().any(|n| line.contains(n.as_str())) {
            out.push(json!({"pid": pid, "line": line}));
        }
    }
}

/// Cgroup directories (relative to `root`) whose name contains a needle; the
/// walk does not descend into a match, nor deeper than `depth`.
pub fn cgroup_dirs(root: &Path, needles: &[String], depth: usize) -> Result<Vec<String>> {
    if !root.is_dir() {
        bail!("cgroup tree {} is not mounted", root.display());
    }
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, level)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            if needles.iter().any(|n| name.contains(n.as_str())) {
                found.push(p.strip_prefix(root).unwrap_or(&p).display().to_string());
            } else if level < depth {
                stack.push((p, level + 1));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The host-side part of the evidence: mounts, cgroup lines, cgroup dirs,
/// kubelet Pod dirs.
pub fn host_evidence(host: &Host, needles: &[String], uids: &[String]) -> Result<Value> {
    if !host.host_mountinfo.is_file() {
        bail!("host mountinfo {} is not mounted", host.host_mountinfo.display());
    }
    if !host.proc_root.join("1").is_dir() {
        bail!("{} shows no pid 1: hostPID or the /proc mount is missing", host.proc_root.display());
    }
    let (mut mounts, mut cgroups) = (Vec::new(), Vec::new());
    matching_lines(&host.host_mountinfo, needles, "host", &mut mounts);
    for e in std::fs::read_dir(&host.proc_root)?.flatten() {
        let pid = e.file_name().to_string_lossy().to_string();
        if pid.chars().all(|c| c.is_ascii_digit()) {
            matching_lines(&e.path().join("mountinfo"), needles, &pid, &mut mounts);
            matching_lines(&e.path().join("cgroup"), needles, &pid, &mut cgroups);
        }
    }
    let mut cg_needles = needles.to_vec();
    cg_needles.extend(uids.iter().map(|u| u.replace('-', "_")));
    let dirs = cgroup_dirs(&host.cgroup_root, &cg_needles, 10)?;
    let (pod_dirs, unmeasured) = match &host.kubelet_pods {
        Some(p) if p.is_dir() => (json!(uids.iter().filter(|u| p.join(u).exists()).collect::<Vec<_>>()), json!([])),
        _ => (Value::Null, json!(["pod_directories"])),
    };
    Ok(json!({"mounts": mounts, "cgroups": cgroups, "cgroup_dirs": dirs,
        "pod_directories": pod_dirs, "unmeasured": unmeasured}))
}

/// The whole evidence for one phase: stormblock's volumes and slots, then
/// the host's.
pub async fn collect(http: &reqwest::Client, host: &Host, req: &Request) -> Result<Value> {
    let token = std::fs::read_to_string(&host.token_file)
        .with_context(|| format!("stormblock token {}", host.token_file.display()))?;
    let token = token.trim();
    let base = host.stormblock.as_str();
    let volumes = stormblock_items(http, base, token, "volumes").await?;
    let owned: Vec<Value> = volumes
        .into_iter()
        .filter(|v| {
            v["name"].as_str().is_some_and(|n| req.names.iter().any(|x| x == n))
                || v["id"].as_str().is_some_and(|i| req.ids.iter().any(|x| x == i))
        })
        .collect();
    let mut ids: Vec<String> = req.ids.clone();
    ids.extend(owned.iter().filter_map(|v| v["id"].as_str().map(str::to_string)));
    ids.sort();
    ids.dedup();
    let slabs = stormblock_items(http, base, token, "slabs").await?;
    let mut slots = Vec::new();
    for slab in &slabs {
        let Some(id) = slab["id"].as_str() else { continue };
        for mut s in stormblock_items(http, base, token, &format!("slabs/{id}/slots")).await? {
            if s["volume_id"].as_str().is_some_and(|v| ids.iter().any(|x| x == v)) {
                s["slab"] = json!(id);
                s["slot_size"] = slab["slot_size"].clone();
                slots.push(s);
            }
        }
    }
    let mut needles = req.uids.clone();
    needles.extend(ids.iter().cloned());
    needles.extend(req.names.iter().cloned());
    needles.retain(|n| !n.is_empty());
    let host_part = {
        let (host, needles, uids) = (host.clone(), needles.clone(), req.uids.clone());
        tokio::task::spawn_blocking(move || host_evidence(&host, &needles, &uids)).await??
    };
    let mut ev = json!({"volumes": owned, "slots": slots, "slabs": slabs});
    for (k, v) in host_part.as_object().into_iter().flatten() {
        ev[k] = v.clone();
    }
    Ok(ev)
}

/// Whether a phase's evidence verifies: `before` records only; `allocated`
/// needs exactly the run's volumes, each with storage allocated; `after`
/// needs nothing of the run left anywhere measured.
pub fn verified(phase: Phase, ev: &Value, names: &[String]) -> bool {
    match phase {
        Phase::Before => true,
        Phase::Allocated => {
            let vols = ev["volumes"].as_array().cloned().unwrap_or_default();
            let mut seen: Vec<&str> = vols.iter().filter_map(|v| v["name"].as_str()).collect();
            seen.sort();
            seen.dedup();
            let mut want: Vec<&str> = names.iter().map(String::as_str).collect();
            want.sort();
            vols.len() == names.len() && seen == want && vols.iter().all(|v| v["allocated_bytes"].as_u64().is_some_and(|b| b > 0))
        }
        Phase::After => ["volumes", "slots", "mounts", "cgroups", "cgroup_dirs", "pod_directories"]
            .iter()
            .all(|k| ev[*k].as_array().is_none_or(|a| a.is_empty())),
    }
}

/// Volume ids the allocated audit saw (so `after` still looks for them).
pub fn volume_ids(ev: &Value) -> Vec<String> {
    ev["volumes"].as_array().into_iter().flatten().filter_map(|v| v["id"].as_str().map(str::to_string)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qa-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cgroup_dirs_match_both_uid_forms_and_stop_at_a_match() {
        let root = tree("cg");
        let uid = "1234-abcd";
        std::fs::create_dir_all(root.join("kubepods.slice/kubepods-pod1234_abcd.slice/cri-x")).unwrap();
        std::fs::create_dir_all(root.join("system.slice/other")).unwrap();
        let needles = vec![uid.to_string(), uid.replace('-', "_")];
        assert_eq!(cgroup_dirs(&root, &needles, 10).unwrap(), vec!["kubepods.slice/kubepods-pod1234_abcd.slice"]);
        assert!(cgroup_dirs(&root.join("absent"), &needles, 10).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_evidence_reads_proc_and_marks_pod_dirs_unmeasured() {
        let root = tree("host");
        let proc_root = root.join("proc");
        std::fs::create_dir_all(proc_root.join("1")).unwrap();
        std::fs::create_dir_all(proc_root.join("42")).unwrap();
        std::fs::create_dir_all(proc_root.join("self")).unwrap();
        std::fs::write(proc_root.join("1/mountinfo"), "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();
        std::fs::write(proc_root.join("42/mountinfo"), "9 1 0:9 / /data rw - ext4 /dev/sb-vol-7 rw\n").unwrap();
        std::fs::write(proc_root.join("42/cgroup"), "0::/kubepods/pod-uid-1/x\n").unwrap();
        let cg = root.join("cg");
        std::fs::create_dir_all(&cg).unwrap();
        let host = Host {
            proc_root: proc_root.clone(),
            host_mountinfo: proc_root.join("1/mountinfo"),
            cgroup_root: cg,
            token_file: root.join("token"),
            stormblock: "http://127.0.0.1:1".into(),
            kubelet_pods: None,
        };
        let needles = vec!["uid-1".to_string(), "vol-7".to_string()];
        let ev = host_evidence(&host, &needles, &["uid-1".to_string()]).unwrap();
        assert_eq!(ev["mounts"].as_array().unwrap().len(), 1);
        assert_eq!(ev["mounts"][0]["pid"], "42");
        assert_eq!(ev["cgroups"].as_array().unwrap().len(), 1);
        assert!(ev["pod_directories"].is_null());
        assert_eq!(ev["unmeasured"], json!(["pod_directories"]));
        // A residue makes `after` fail; unmeasured alone does not.
        assert!(!verified(Phase::After, &ev, &[]));
        let clean = json!({"volumes": [], "slots": [], "mounts": [], "cgroups": [], "cgroup_dirs": [], "pod_directories": null});
        assert!(verified(Phase::After, &clean, &[]));
        // Missing token: the Job is not the owner's spec.
        assert!(host.unusable().unwrap().contains("token"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn allocated_needs_exactly_the_runs_volumes_with_storage() {
        let names = vec!["pvc-ns-a".to_string(), "pvc-ns-b".to_string()];
        let ev = |b: u64| json!({"volumes": [
            {"name": "pvc-ns-a", "id": "1", "allocated_bytes": 4096},
            {"name": "pvc-ns-b", "id": "2", "allocated_bytes": b}]});
        assert!(verified(Phase::Allocated, &ev(8192), &names));
        assert!(!verified(Phase::Allocated, &ev(0), &names));
        assert!(!verified(Phase::Allocated, &ev(8192), &names[..1]));
        assert_eq!(volume_ids(&ev(1)), vec!["1", "2"]);
    }
}
