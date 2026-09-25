//! What exists on the node, counted before the first wave and after each
//! drain. Two readings per metric where it can be told apart: **node-wide**
//! (for the trend — a leak that grows wave on wave) and **this run's own**
//! (which must be exactly zero after a drain).
//!
//! Sources, API first:
//! - apiserver: VirtualMachines / VMIs left in the run's namespace;
//! - stormblock (`<node>:9090/api/v1/volumes`, `…/{id}/attach`): volumes and
//!   attachments. The kubelet names a VM's clone `<ns>.<vm>-<disk>` and its
//!   seed `<vm>-seed`; nothing else ties a volume to a VM;
//! - stormvm (`127.0.0.1:9095/api/v1/vms`, loopback-only on stormcos, so
//!   reachable only because the Job runs `hostNetwork`): VM registrations;
//! - the host's `/proc` (hostNetwork again), where no API exists: taps
//!   (`vm%08x` from `/proc/net/dev`), used memory (`/proc/meminfo`) and
//!   allocated file handles (`/proc/sys/fs/file-nr`).
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
}

impl Own {
    pub fn is_empty(&self) -> bool {
        *self == Own::default()
    }
}

pub struct Sources<'a> {
    pub kube: &'a Client,
    pub http: &'a reqwest::Client,
    pub namespace: &'a str,
    pub stormblock: &'a str,
    pub stormvm: &'a str,
    pub proc_root: &'a str,
    /// The VM names this run uses (every wave reuses them).
    pub vm_names: &'a [String],
}

impl Sources<'_> {
    async fn json(&self, url: &str) -> Option<Value> {
        let r = self.http.get(url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }

    fn own_volume(&self, name: &str) -> bool {
        name.starts_with(&format!("{}.", self.namespace))
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
    fn memory() {
        let m = "MemTotal:       1000 kB\nMemFree: 1 kB\nMemAvailable:    400 kB\n";
        assert_eq!(mem_used(m), Some(600 * 1024));
        assert_eq!(mem_available(m), Some(400 * 1024));
    }
}
