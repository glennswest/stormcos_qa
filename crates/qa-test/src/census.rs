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
//! - stormblock (`<engine>/api/v1/volumes`, `…/{id}/attach`, engine.rs): volumes and
//!   attachments. The kubelet names a VM's clone `<ns>.<vm>-<disk>`, its
//!   seed `<vm>-seed`, and a `stormblock`-class claim's clone `pvc-<ns>-<claim>`;
//!   and `…/api/v1/slabs`: bytes and slots allocated across the slabs, so
//!   space a deleted volume never gave back shows even when the volume count
//!   is flat (#36);
//! - stormvm (`127.0.0.1:9095/api/v1/vms`, loopback-only on stormcos, so
//!   reachable only because the Job runs `hostNetwork`): VM registrations;
//! - the host's `/proc` (hostNetwork again), where no API exists: taps
//!   (`vm%08x` from `/proc/net/dev`), pod veths (`lxc*`/`veth*`, same file),
//!   only when `/proc/net/dev` is the host's (see [`host_netns`]),
//!   cgroups (`/proc/cgroups`, the largest `num_cgroups`), used memory
//!   (`/proc/meminfo`), allocated file handles (`/proc/sys/fs/file-nr`), and
//!   the stormblock engine's own resident memory (`/proc/<pid>/status`
//!   `VmRSS` of `stormblock adopt-ublk`: needs `hostPID`; node memory alone
//!   can hide it behind a guest's page cache, #36);
//! - the run namespace's Secrets but the run's own key: a VM's cloud-init
//!   seed is inline `userData` today, so any Secret a VM or the platform
//!   leaves there is residue (#36).
//!
//! A source that cannot be read gives `None`, reported as unmeasured —
//! never a silent zero. A leftover list that cannot be read stays empty in
//! [`Own`] but is named in [`Census::unread`] with why (#95, finding
//! 70732200b087): a drain's "nothing left" holds only over sources that
//! were read ([`Census::unread_of`]).

use std::collections::BTreeMap;

use serde::Serialize;
use retry::Policy;
use retry::http::{self as rhttp, Repeat};
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
    /// Bytes (and slots) allocated across stormblock's slabs.
    pub slab_allocated_bytes: Option<u64>,
    pub slab_allocated_slots: Option<u64>,
    /// The stormblock engine's resident memory.
    pub engine_rss_bytes: Option<u64>,
    /// This run's own leftovers, by name.
    pub own: Own,
    /// The leftover sources this census could not read, by name
    /// ([`VM_SOURCES`], [`POD_SOURCES`]), with why: their [`Own`] list is
    /// empty because it is unknown, not because nothing is there.
    pub unread: BTreeMap<&'static str, String>,
}

/// The leftover sources a VM wave's drain is judged on.
pub const VM_SOURCES: &[&str] = &["vms", "vmis", "volumes", "registrations", "taps", "secrets"];
/// The leftover sources a container wave's drain is judged on.
pub const POD_SOURCES: &[&str] = &["deployments", "replicasets", "pods", "pvcs", "services", "pvs", "volumes", "secrets"];

impl Census {
    /// Which of `sources` this census could not read, `(source, why)`.
    pub fn unread_of(&self, sources: &[&'static str]) -> BTreeMap<&'static str, String> {
        self.unread.iter().filter(|(k, _)| sources.contains(k)).map(|(k, v)| (*k, v.clone())).collect()
    }
}

/// `sources` less those already unreadable in `baseline`: those were
/// reported as could-not-run once, before the first wave; a drain waits on
/// every other one being read.
pub fn needed(sources: &[&'static str], baseline: &Census) -> Vec<&'static str> {
    sources.iter().copied().filter(|s| !baseline.unread.contains_key(s)).collect()
}

/// What a drain left: the run's leftovers, and the needed sources its last
/// census could not read. Clean only when both are empty.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Drained {
    pub own: Own,
    pub unread: BTreeMap<&'static str, String>,
}

impl Drained {
    pub fn of(c: Census, needed: &[&'static str]) -> Drained {
        Drained { unread: c.unread_of(needed), own: c.own }
    }
    /// Verified clean: nothing left, and every needed source read.
    pub fn is_clean(&self) -> bool {
        self.own.is_empty() && self.unread.is_empty()
    }
    /// The verdict: `Ok(())` clean, `Err(Some(left))` something left (a
    /// failure, whatever else was unread), `Err(None)` nothing seen but a
    /// needed source unread (could not run).
    pub fn verdict(&self) -> Result<(), Option<&Own>> {
        if !self.own.is_empty() {
            Err(Some(&self.own))
        } else if !self.unread.is_empty() {
            Err(None)
        } else {
            Ok(())
        }
    }
    pub fn unread_text(&self) -> String {
        self.unread.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("; ")
    }
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
    /// Secrets in the run's namespace other than its own key.
    pub secrets: Vec<String>,
}

impl Own {
    pub fn is_empty(&self) -> bool {
        *self == Own::default()
    }
}

/// stormblock's management token, found the way its own CLI finds it
/// (stormblock#107): `$STORMBLOCK_API_TOKEN`, the file at
/// `$STORMBLOCK_TOKEN_FILE`, `/etc/stormblock/api_token`,
/// `/var/lib/stormblock/api_token`; then the engine's own file on the node,
/// `/run/stormblock/engine/api_token`, under `$STORM_HOST_ROOT` when the
/// runner mounts it read-only (stormcentral#74). Without it every volume
/// call is a 401.
pub fn stormblock_token() -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(t) = env("STORMBLOCK_API_TOKEN") {
        return Some(t.trim().to_string());
    }
    let files = env("STORMBLOCK_TOKEN_FILE")
        .into_iter()
        .chain(["/etc/stormblock/api_token".to_string(), "/var/lib/stormblock/api_token".to_string()])
        .chain(env("STORM_HOST_ROOT").map(|h| format!("{}/run/stormblock/engine/api_token", h.trim_end_matches('/'))))
        .chain(["/run/stormblock/engine/api_token".to_string()]);
    files
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// Is the volume named `name` on stormblock at `base`? stormblock's
/// `GET /api/v1/volumes/{id}` takes only a UUID, so a lookup by name is a 400
/// whether or not the volume exists (stormblock#112): list and match the name.
/// `Err` when the list is refused (its status) or unreachable (a
/// `retry::Infra`).
pub async fn volume_named(http: &reqwest::Client, base: &str, name: &str) -> anyhow::Result<bool> {
    let url = format!("{}/api/v1/volumes", base.trim_end_matches('/'));
    let r = rhttp::send(&Policy::API, &format!("stormblock GET {url}"), Repeat::Safe, || http.get(&url)).await?;
    anyhow::ensure!((200..300).contains(&r.code), "{url} answered {}", r.code);
    let v: Value = serde_json::from_slice(&r.body).map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    Ok(has_volume(&v, name))
}

/// stormblock's open `GET /api/v1/health` (#5): where the node runs from
/// (`slabs`, stormblock#322) and `flow_over_remaining`. `Err` when it is
/// refused (its status) or unreachable (a `retry::Infra`).
pub async fn stormblock_health(http: &reqwest::Client, base: &str) -> anyhow::Result<Value> {
    let url = format!("{}/api/v1/health", base.trim_end_matches('/'));
    let r = rhttp::send(&Policy::API, &format!("stormblock GET {url}"), Repeat::Safe, || http.get(&url)).await?;
    anyhow::ensure!((200..300).contains(&r.code), "{url} answered {}", r.code);
    serde_json::from_slice(&r.body).map_err(|e| anyhow::anyhow!("{url}: {e}"))
}

fn has_volume(list: &Value, name: &str) -> bool {
    kube::items(list).iter().any(|i| i["name"].as_str() == Some(name))
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
    /// The VMs' host bridge: its presence says `/proc/net/dev` is the host's.
    pub bridge: &'a str,
    /// The VM names this run uses (every wave reuses them).
    pub vm_names: &'a [String],
    /// The run's own key Secret (and `<it>-agent`), which stay for the whole run.
    pub run_secret: String,
}

impl Sources<'_> {
    async fn json(&self, url: &str) -> Result<Value, String> {
        let http = if url.starts_with(self.stormblock) { self.stormblock_http } else { self.http };
        let r = rhttp::send(&Policy::API, &format!("GET {url}"), Repeat::Safe, || http.get(url)).await.map_err(|e| format!("{url}: {e:#}"))?;
        if !(200..300).contains(&r.code) {
            return Err(format!("{url} answered {}", r.code));
        }
        serde_json::from_slice(&r.body).map_err(|e| format!("{url}: {e}"))
    }

    /// An apiserver list, or why it could not be read.
    async fn list(&self, path: &str) -> Result<Value, String> {
        match self.kube.get(path).await {
            Ok(r) if r.ok() => Ok(r.body),
            Ok(r) => Err(format!("{path} answered {}", r.code)),
            Err(e) => Err(format!("{path}: {e:#}")),
        }
    }

    fn own_volume(&self, name: &str) -> bool {
        name.starts_with(&format!("{}.", self.namespace))
            || name.starts_with(&format!("pvc-{}-", self.namespace))
            || self.vm_names.iter().any(|vm| name == format!("{vm}-seed"))
    }

    pub async fn take(&self) -> Census {
        let mut c = Census::default();
        for (path, src) in [(kube::vms(self.namespace), "vms"), (kube::vmis(self.namespace), "vmis")] {
            match self.list(&path).await {
                Ok(v) if src == "vms" => c.own.vms = names(&v),
                Ok(v) => c.own.vmis = names(&v),
                Err(why) => {
                    c.unread.insert(src, why);
                }
            }
        }

        let ns = self.namespace;
        let sel = format!("?labelSelector=app.kubernetes.io/managed-by%3D{}", crate::containers::MANAGED_BY);
        let lists = [
            ("deployments", format!("/apis/apps/v1/namespaces/{ns}/deployments{sel}"), &mut c.own.deployments),
            ("replicasets", format!("/apis/apps/v1/namespaces/{ns}/replicasets{sel}"), &mut c.own.replicasets),
            ("pods", format!("/api/v1/namespaces/{ns}/pods{sel}"), &mut c.own.pods),
            ("pvcs", format!("/api/v1/namespaces/{ns}/persistentvolumeclaims{sel}"), &mut c.own.pvcs),
            ("services", format!("/api/v1/namespaces/{ns}/services{sel}"), &mut c.own.services),
        ];
        for (src, path, out) in lists {
            match self.list(&path).await {
                Ok(v) => *out = names(&v),
                Err(why) => {
                    c.unread.insert(src, why);
                }
            }
        }
        match self.list("/api/v1/persistentvolumes").await {
            Err(why) => {
                c.unread.insert("pvs", why);
            }
            Ok(v) => {
                let pvs = kube::items(&v);
                c.pvs = Some(pvs.len());
                c.own.pvs = pvs
                    .iter()
                    .filter(|p| p["spec"]["claimRef"]["namespace"].as_str() == Some(ns))
                    .filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string))
                    .collect();
            }
        }

        match self.json(&format!("{}/api/v1/volumes", self.stormblock)).await {
            Err(why) => {
                c.unread.insert("volumes", why);
            }
            Ok(v) => {
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
                        Ok(a) if a["attached"].as_bool() == Some(true) => attached += 1,
                        Ok(_) => {}
                        Err(_) => known = false,
                    }
                }
                c.attachments = known.then_some(attached);
            }
        }

        if let Ok(v) = self.json(&format!("{}/api/v1/slabs", self.stormblock)).await {
            let (bytes, slots) = slab_allocated(&v);
            c.slab_allocated_bytes = Some(bytes);
            c.slab_allocated_slots = Some(slots);
        }

        match self.list(&format!("/api/v1/namespaces/{}/secrets", self.namespace)).await {
            Err(why) => {
                c.unread.insert("secrets", why);
            }
            Ok(v) => {
                c.own.secrets = kube::items(&v)
                    .iter()
                    .filter(|s| s["type"] != "kubernetes.io/service-account-token")
                    .filter_map(|s| s["metadata"]["name"].as_str())
                    // The run's key Secret and its `-agent` twin (#52) stay for
                    // the whole run.
                    .filter(|n| !n.starts_with(self.run_secret.as_str()))
                    .map(str::to_string)
                    .collect();
            }
        }

        match self.json(&format!("{}/api/v1/vms", self.stormvm)).await {
            Err(why) => {
                c.unread.insert("registrations", why);
            }
            Ok(v) => {
                let items = kube::items(&v);
                c.registrations = Some(items.len());
                c.own.registrations = items
                    .iter()
                    .filter(|i| i["namespace"].as_str() == Some(self.namespace))
                    .filter_map(|i| i["name"].as_str().map(str::to_string))
                    .collect();
            }
        }

        // Only the host's network namespace shows taps and veths; a pod's
        // own shows lo and eth0, which would read as a wrong zero.
        match tokio::fs::read_to_string(format!("{}/net/dev", self.proc_root)).await {
            Ok(dev) if host_netns(&dev, self.bridge) => {
                let taps = parse_taps(&dev);
                let mine: Vec<String> = self.vm_names.iter().map(|vm| tap_name(self.namespace, vm, "default")).collect();
                c.own.taps = taps.iter().filter(|t| mine.contains(t)).cloned().collect();
                c.taps = Some(taps.len());
                c.veths = Some(parse_veths(&dev));
            }
            Ok(_) => {
                c.unread.insert("taps", format!("{}/net/dev is not the host's network namespace (no {} bridge)", self.proc_root, self.bridge));
            }
            Err(e) => {
                c.unread.insert("taps", format!("{}/net/dev: {e}", self.proc_root));
            }
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
        c.engine_rss_bytes = engine_rss(self.proc_root);
        c
    }
}

/// Bytes and slots allocated across every slab in a `GET /api/v1/slabs` list.
pub fn slab_allocated(list: &Value) -> (u64, u64) {
    kube::items(list).iter().fold((0, 0), |(b, s), i| {
        let slots = i["allocated_slots"].as_u64().unwrap_or(0);
        (b + slots * i["slot_size"].as_u64().unwrap_or(0), s + slots)
    })
}

/// The stormblock engine's `VmRSS`: the process whose command line is
/// `stormblock … adopt-ublk`, found in a `/proc` that shows the host's
/// processes (`hostPID`). `None` when it is not visible.
pub fn engine_rss(proc_root: &str) -> Option<u64> {
    let rd = std::fs::read_dir(proc_root).ok()?;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().filter(|n| n.bytes().all(|b| b.is_ascii_digit())) else { continue };
        let Ok(cmd) = std::fs::read(format!("{proc_root}/{pid}/cmdline")) else { continue };
        if is_engine(&cmd) {
            return std::fs::read_to_string(format!("{proc_root}/{pid}/status")).ok().and_then(|s| vm_rss(&s));
        }
    }
    None
}

/// `/proc/<pid>/cmdline` (NUL-separated) of the engine: its program is
/// `stormblock` and one argument is `adopt-ublk`.
pub fn is_engine(cmdline: &[u8]) -> bool {
    let mut args = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty());
    let prog = args.next().unwrap_or_default();
    prog.rsplit(|b| *b == b'/').next() == Some(b"stormblock".as_slice()) && args.any(|a| a == b"adopt-ublk")
}

/// `VmRSS:` from `/proc/<pid>/status`, in bytes.
pub fn vm_rss(status: &str) -> Option<u64> {
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()).map(|kb| kb * 1024)
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

/// Whether a `/proc/net/dev` is the host's network namespace (the Job runs
/// `hostNetwork`): the VMs' bridge, or Cilium's host-side interfaces, are in it.
pub fn host_netns(proc_net_dev: &str, bridge: &str) -> bool {
    proc_net_dev
        .lines()
        .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim()))
        .any(|n| n == bridge || n == "cilium_host" || n == "lxc_health")
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
    fn slab_allocation_engine_and_rss() {
        let slabs = serde_json::json!({"items": [
            {"allocated_slots": 10, "slot_size": 1048576, "free_slots": 90},
            {"allocated_slots": 3, "slot_size": 4096}], "count": 2});
        assert_eq!(slab_allocated(&slabs), (10 * 1048576 + 3 * 4096, 13));
        assert!(is_engine(b"/usr/bin/stormblock\0adopt-ublk\0--api\x000.0.0.0:9090\0"));
        assert!(!is_engine(b"/usr/bin/stormblock\0must-gather\0"));
        assert!(!is_engine(b"/usr/bin/stormblock-csi\0adopt-ublk\0"));
        assert_eq!(vm_rss("Name:\tstormblock\nVmRSS:\t  204800 kB\nThreads:\t9\n"), Some(204800 * 1024));
        // A fake /proc: the engine among other processes.
        let p = std::env::temp_dir().join(format!("census-proc-{}", std::process::id()));
        for (pid, cmd, rss) in [("1", "/sbin/stormpump\0", 4000), ("77", "/usr/bin/stormblock\0adopt-ublk\0", 512000)] {
            std::fs::create_dir_all(p.join(pid)).unwrap();
            std::fs::write(p.join(pid).join("cmdline"), cmd).unwrap();
            std::fs::write(p.join(pid).join("status"), format!("VmRSS:\t{rss} kB\n")).unwrap();
        }
        assert_eq!(engine_rss(p.to_str().unwrap()), Some(512000 * 1024));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn a_volume_is_found_by_name_in_the_list() {
        let list = serde_json::json!({"items": [{"id": "6f1c", "name": "fedora-44-x86_64"}, {"id": "9a2b", "name": "ns.vm-root"}], "count": 2});
        assert!(has_volume(&list, "fedora-44-x86_64"));
        assert!(!has_volume(&list, "fedora-44"));
        assert!(!has_volume(&serde_json::json!({"items": []}), "fedora-44-x86_64"));
    }

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
    fn only_the_hosts_netns_counts() {
        assert!(host_netns("    lo: 1\nstormbr0: 1\n", "stormbr0"));
        assert!(host_netns("    lo: 1\ncilium_host: 1\n", "stormbr0"));
        // A pod's own namespace: taps and veths would read as a wrong 0.
        assert!(!host_netns("    lo: 1\n  eth0: 1\n", "stormbr0"));
    }

    #[test]
    fn veths_and_cgroups() {
        let dev = "    lo: 1\nlxc_health: 1\nlxc1a2b: 1\nveth9: 1\nvm0a1b2c3d: 1\neth0: 1\n";
        assert_eq!(parse_veths(dev), 3);
        let cg = "#subsys_name\thierarchy\tnum_cgroups\tenabled\ncpu\t0\t41\t1\nmemory\t0\t43\t1\n";
        assert_eq!(parse_cgroups(cg), Some(43));
        assert_eq!(parse_cgroups("#only a header\n"), None);
    }

    /// A stand-in apiserver + stormblock + stormvm on one port: `refuse`d
    /// paths answer 403, every other one an empty list.
    async fn stand_in(refuse: &'static [&'static str]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
                    let (code, body) = if refuse.contains(&path.as_str()) { ("403 Forbidden", "{}") } else { ("200 OK", r#"{"items": []}"#) };
                    let _ = s.write_all(format!("HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        at
    }

    async fn census_of(at: &str, proc_root: &str) -> Census {
        let kube = Client::new(at, Some("/nonexistent-token"), false).await.unwrap();
        let http = reqwest::Client::new();
        let vm_names = vec!["vm-1".to_string()];
        let sources = Sources {
            kube: &kube,
            http: &http,
            stormblock_http: &http,
            namespace: "ns",
            stormblock: at,
            stormvm: at,
            proc_root,
            bridge: "stormbr0",
            vm_names: &vm_names,
            run_secret: "key".into(),
        };
        sources.take().await
    }

    /// Finding 70732200b087 (#95): a list that cannot be read is named in
    /// `unread`, and a drain over it is not clean, though its `Own` list is
    /// empty; with every source read, the same empty census is clean.
    #[tokio::test]
    async fn an_unreadable_source_is_not_an_empty_drain() {
        let proc_root = std::env::temp_dir().join(format!("census-unread-{}", std::process::id()));
        std::fs::create_dir_all(proc_root.join("net")).unwrap();
        std::fs::write(proc_root.join("net/dev"), "    lo: 1\nstormbr0: 1\n").unwrap();
        let pr = proc_root.to_str().unwrap();

        let at = stand_in(&[]).await;
        let c = census_of(&at, pr).await;
        assert!(c.unread.is_empty(), "all read: {:?}", c.unread);
        assert!(Drained::of(c.clone(), VM_SOURCES).is_clean());
        assert!(Drained::of(c, POD_SOURCES).is_clean());

        let at = stand_in(&["/api/v1/namespaces/ns/pods", "/api/v1/volumes", "/api/v1/vms"]).await;
        let c = census_of(&at, pr).await;
        assert!(c.own.is_empty(), "nothing seen");
        assert_eq!(c.unread.keys().copied().collect::<Vec<_>>(), ["pods", "registrations", "volumes"]);
        assert!(c.unread["pods"].contains("403"), "{}", c.unread["pods"]);
        let pods = Drained::of(c.clone(), POD_SOURCES);
        assert!(!pods.is_clean());
        assert_eq!(pods.verdict(), Err(None), "nothing seen, but not verified: could not run");
        assert_eq!(pods.unread.keys().copied().collect::<Vec<_>>(), ["pods", "volumes"]);
        let vms = Drained::of(c.clone(), VM_SOURCES);
        assert_eq!(vms.unread.keys().copied().collect::<Vec<_>>(), ["registrations", "volumes"]);
        // A source unreadable since the baseline was reported then; the
        // drain does not wait on it again.
        assert_eq!(needed(VM_SOURCES, &c), ["vms", "vmis", "taps", "secrets"]);
        assert!(Drained::of(c.clone(), &needed(VM_SOURCES, &c)).is_clean());

        // /proc/net/dev not the host's: taps are unread, not zero.
        std::fs::write(proc_root.join("net/dev"), "    lo: 1\n  eth0: 1\n").unwrap();
        let at = stand_in(&[]).await;
        let c = census_of(&at, pr).await;
        assert_eq!(c.unread.keys().copied().collect::<Vec<_>>(), ["taps"]);
        assert_eq!(c.taps, None);
        let _ = std::fs::remove_dir_all(&proc_root);
    }

    #[test]
    fn something_left_is_a_failure_whatever_else_was_unread() {
        let mut d = Drained::default();
        assert_eq!(d.verdict(), Ok(()));
        d.unread.insert("pods", "answered 403".into());
        assert_eq!(d.verdict(), Err(None));
        assert_eq!(d.unread_text(), "pods: answered 403");
        d.own.vms.push("vm-1".into());
        assert_eq!(d.verdict(), Err(Some(&d.own.clone())));
    }

    #[test]
    fn memory() {
        let m = "MemTotal:       1000 kB\nMemFree: 1 kB\nMemAvailable:    400 kB\n";
        assert_eq!(mem_used(m), Some(600 * 1024));
        assert_eq!(mem_available(m), Some(400 * 1024));
    }
}
