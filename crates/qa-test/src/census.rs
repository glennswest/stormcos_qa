//! What exists on the node, counted before the first wave and after each
//! drain. Two readings per metric where it can be told apart: **node-wide**
//! (for the trend — a leak that grows wave on wave) and **this run's own**
//! (which must be exactly zero after a drain).
//!
//! Sources, API first:
//! - apiserver: VirtualMachines / VMIs left in the run's namespace, and the
//!   container waves' Deployments, ReplicaSets, pods, claims and Services
//!   (by their `app.kubernetes.io/managed-by` label, so the Job's own pod is
//!   not counted) and PersistentVolumes bound to the namespace (cluster read
//!   of `persistentvolumes`, test/requires.toml);
//! - stormblock (`<node>:9090/api/v1/volumes`, `…/{id}/attach`): volumes and
//!   attachments. The kubelet names a VM's clone `<ns>.<vm>-<disk>`, its
//!   seed `<vm>-seed`, and a `stormblock`-class claim's clone `pvc-<ns>-<claim>`;
//! - stormvm (`127.0.0.1:9095/api/v1/vms`, loopback-only on stormcos, so
//!   reachable only because the Job runs `hostNetwork`): VM registrations;
//! - the host's `/proc` (hostNetwork again), where no API exists: taps
//!   (`vm%08x` from `/proc/net/dev`), pod veths (`lxc*`/`veth*`, same file),
//!   cgroups (`/proc/cgroups`, the largest `num_cgroups`), used memory
//!   (`/proc/meminfo`) and allocated file handles (`/proc/sys/fs/file-nr`).
//!
//! A source that cannot be read gives `None`, reported as unmeasured —
//! never a silent zero.

use serde::Serialize;
use serde_json::Value;

use crate::kube::{self, Client};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Census {
    pub volumes: Option<usize>,
    pub attachments: Option<usize>,
    pub registrations: Option<usize>,
    pub taps: Option<usize>,
    pub veths: Option<usize>,
    pub cgroups: Option<u64>,
    /// PersistentVolumes cluster-wide.
    pub pvs: Option<usize>,
    pub mem_used_bytes: Option<u64>,
    pub fds: Option<u64>,
    /// This run's own leftovers, by name.
    pub own: Own,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Own {
    pub vms: Vec<String>,
    pub vmis: Vec<String>,
    pub volumes: Vec<String>,
    pub registrations: Vec<String>,
    pub taps: Vec<String>,
    pub deployments: Vec<String>,
    pub replicasets: Vec<String>,
    pub pods: Vec<String>,
    pub pvcs: Vec<String>,
    pub services: Vec<String>,
    /// PVs whose claimRef is in the run's namespace.
    pub pvs: Vec<String>,
}

impl Own {
    pub fn is_empty(&self) -> bool {
        *self == Own::default()
    }
}

/// stormblock's management token, found the way its own CLI finds it
/// (stormblock#107): `$STORMBLOCK_API_TOKEN`, the file at
/// `$STORMBLOCK_TOKEN_FILE`, `/etc/stormblock/api_token`,
/// `/var/lib/stormblock/api_token`. Without it every volume call is a 401.
pub fn stormblock_token() -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(t) = env("STORMBLOCK_API_TOKEN") {
        return Some(t.trim().to_string());
    }
    let files = env("STORMBLOCK_TOKEN_FILE")
        .into_iter()
        .chain(["/etc/stormblock/api_token".to_string(), "/var/lib/stormblock/api_token".to_string()]);
    files
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// A client that sends stormblock's token on every request, when there is one.
pub fn stormblock_client(token: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(t) = token {
        let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))?;
        v.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    Ok(reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).default_headers(headers).build()?)
}

pub struct Sources<'a> {
    pub kube: &'a Client,
    pub http: &'a reqwest::Client,
    /// stormblock's client: `http` plus the engine's bearer token.
    pub stormblock_http: &'a reqwest::Client,
    pub namespace: &'a str,
    pub stormblock: &'a str,
    pub stormvm: &'a str,
    pub proc_root: &'a str,
    /// The VM names this run uses (every wave reuses them).
    pub vm_names: &'a [String],
}

impl Sources<'_> {
    async fn json(&self, url: &str) -> Option<Value> {
        let http = if url.starts_with(self.stormblock) { self.stormblock_http } else { self.http };
        let r = http.get(url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }

    fn own_volume(&self, name: &str) -> bool {
        name.starts_with(&format!("{}.", self.namespace))
            || name.starts_with(&format!("pvc-{}-", self.namespace))
            || self.vm_names.iter().any(|vm| name == format!("{vm}-seed"))
    }

    pub async fn take(&self) -> Census {
        let mut c = Census::default();
        for (path, out) in [(kube::vms(self.namespace), 0), (kube::vmis(self.namespace), 1)] {
            if let Ok(r) = self.kube.get(&path).await
                && r.ok()
            {
                let names: Vec<String> =
                    kube::items(&r.body).iter().filter_map(|i| i["metadata"]["name"].as_str().map(str::to_string)).collect();
                if out == 0 {
                    c.own.vms = names;
                } else {
                    c.own.vmis = names;
                }
            }
        }

        let ns = self.namespace;
        let sel = format!("?labelSelector=app.kubernetes.io/managed-by%3D{}", crate::containers::MANAGED_BY);
        let lists = [
            (format!("/apis/apps/v1/namespaces/{ns}/deployments{sel}"), &mut c.own.deployments),
            (format!("/apis/apps/v1/namespaces/{ns}/replicasets{sel}"), &mut c.own.replicasets),
            (format!("/api/v1/namespaces/{ns}/pods{sel}"), &mut c.own.pods),
            (format!("/api/v1/namespaces/{ns}/persistentvolumeclaims{sel}"), &mut c.own.pvcs),
            (format!("/api/v1/namespaces/{ns}/services{sel}"), &mut c.own.services),
        ];
        for (path, out) in lists {
            if let Ok(r) = self.kube.get(&path).await
                && r.ok()
            {
                *out = names(&r.body);
            }
        }
        if let Ok(r) = self.kube.get("/api/v1/persistentvolumes").await
            && r.ok()
        {
            let pvs = kube::items(&r.body);
            c.pvs = Some(pvs.len());
            c.own.pvs = pvs
                .iter()
                .filter(|p| p["spec"]["claimRef"]["namespace"].as_str() == Some(ns))
                .filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string))
                .collect();
        }

        if let Some(v) = self.json(&format!("{}/api/v1/volumes", self.stormblock)).await {
            let items = kube::items(&v);
            c.volumes = Some(items.len());
            c.own.volumes = items
                .iter()
                .filter_map(|i| i["name"].as_str())
                .filter(|n| self.own_volume(n))
                .map(str::to_string)
                .collect();
            let mut attached = 0;
            let mut known = true;
            for id in items.iter().filter_map(|i| i["id"].as_str()) {
                match self.json(&format!("{}/api/v1/volumes/{id}/attach", self.stormblock)).await {
                    Some(a) if a["attached"].as_bool() == Some(true) => attached += 1,
                    Some(_) => {}
                    None => known = false,
                }
            }
            c.attachments = known.then_some(attached);
        }

        if let Some(v) = self.json(&format!("{}/api/v1/vms", self.stormvm)).await {
            let items = kube::items(&v);
            c.registrations = Some(items.len());
            c.own.registrations = items
                .iter()
                .filter(|i| i["namespace"].as_str() == Some(self.namespace))
                .filter_map(|i| i["name"].as_str().map(str::to_string))
                .collect();
        }

        if let Ok(dev) = tokio::fs::read_to_string(format!("{}/net/dev", self.proc_root)).await {
            let taps = parse_taps(&dev);
            let mine: Vec<String> = self.vm_names.iter().map(|vm| tap_name(self.namespace, vm, "default")).collect();
            c.own.taps = taps.iter().filter(|t| mine.contains(t)).cloned().collect();
            c.taps = Some(taps.len());
            c.veths = Some(parse_veths(&dev));
        }
        if let Ok(cg) = tokio::fs::read_to_string(format!("{}/cgroups", self.proc_root)).await {
            c.cgroups = parse_cgroups(&cg);
        }
        if let Ok(m) = tokio::fs::read_to_string(format!("{}/meminfo", self.proc_root)).await {
            c.mem_used_bytes = mem_used(&m);
        }
        if let Ok(f) = tokio::fs::read_to_string(format!("{}/sys/fs/file-nr", self.proc_root)).await {
            c.fds = f.split_whitespace().next().and_then(|n| n.parse().ok());
        }
        c
    }
}

/// stormvm's tap name: `vm` + FNV-1a-32 of `<ns>/<vm>/<nic>` in hex
/// (stormvm-net).
pub fn tap_name(ns: &str, vm: &str, nic: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in format!("{ns}/{vm}/{nic}").bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("vm{h:08x}")
}

pub fn parse_taps(proc_net_dev: &str) -> Vec<String> {
    proc_net_dev
        .lines()
        .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim()))
        .filter(|n| n.len() == 10 && n.starts_with("vm") && n[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect()
}

fn names(list: &Value) -> Vec<String> {
    kube::items(list).iter().filter_map(|i| i["metadata"]["name"].as_str().map(str::to_string)).collect()
}

/// Pod network interfaces on the host: Cilium's `lxc*`, or plain `veth*`.
pub fn parse_veths(proc_net_dev: &str) -> usize {
    proc_net_dev
        .lines()
        .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim()))
        .filter(|n| n.starts_with("lxc") || n.starts_with("veth"))
        .count()
}

/// The largest `num_cgroups` in `/proc/cgroups` (host-wide, even from a pod).
pub fn parse_cgroups(proc_cgroups: &str) -> Option<u64> {
    proc_cgroups
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().nth(2)?.parse::<u64>().ok())
        .max()
}

/// `MemTotal - MemAvailable`, in bytes.
pub fn mem_used(meminfo: &str) -> Option<u64> {
    let field = |k: &str| {
        meminfo
            .lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
    };
    Some((field("MemTotal:")? - field("MemAvailable:")?) * 1024)
}

/// `MemAvailable`, in bytes.
pub fn mem_available(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse::<u64>().ok())
        .map(|k| k * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_names_are_fnv1a() {
        // FNV-1a-32 of "" is the offset basis.
        let t = tap_name("a", "b", "c");
        assert_eq!(t.len(), 10);
        assert!(t.starts_with("vm"));
        assert_ne!(t, tap_name("a", "b", "d"));
    }

    #[test]
    fn taps_from_proc_net_dev() {
        let dev = "Inter-|   Receive\n face |bytes\n    lo: 1 2\nvm0a1b2c3d: 5 6\nstormbr0: 1\nvmxx: 1\n";
        assert_eq!(parse_taps(dev), vec!["vm0a1b2c3d".to_string()]);
    }

    #[test]
    fn veths_and_cgroups() {
        let dev = "    lo: 1\nlxc_health: 1\nlxc1a2b: 1\nveth9: 1\nvm0a1b2c3d: 1\neth0: 1\n";
        assert_eq!(parse_veths(dev), 3);
        let cg = "#subsys_name\thierarchy\tnum_cgroups\tenabled\ncpu\t0\t41\t1\nmemory\t0\t43\t1\n";
        assert_eq!(parse_cgroups(cg), Some(43));
        assert_eq!(parse_cgroups("#only a header\n"), None);
    }

    #[test]
    fn memory() {
        let m = "MemTotal:       1000 kB\nMemFree: 1 kB\nMemAvailable:    400 kB\n";
        assert_eq!(mem_used(m), Some(600 * 1024));
        assert_eq!(mem_available(m), Some(400 * 1024));
    }
}
