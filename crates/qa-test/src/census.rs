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
    /// Bytes (and slots) allocated across stormblock's slabs.
    pub slab_allocated_bytes: Option<u64>,
    pub slab_allocated_slots: Option<u64>,
    /// The stormblock engine's resident memory.
    pub engine_rss_bytes: Option<u64>,
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
/// `Err` is the HTTP status (or transport error) when the list is refused.
pub async fn volume_named(http: &reqwest::Client, base: &str, name: &str) -> Result<bool, String> {
    let r = http.get(format!("{}/api/v1/volumes", base.trim_end_matches('/'))).send().await.map_err(|e| format!("{base}: {e}"))?;
    let status = r.status();
    if !status.is_success() {
        return Err(status.to_string());
    }
    let v: Value = r.json().await.map_err(|e| format!("{base}/api/v1/volumes: {e}"))?;
    Ok(has_volume(&v, name))
}

fn has_volume(list: &Value, name: &str) -> bool {
    kube::items(list).iter().any(|i| i["name"].as_str() == Some(name))
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
    /// The VMs' host bridge: its presence says `/proc/net/dev` is the host's.
    pub bridge: &'a str,
    /// The VM names this run uses (every wave reuses them).
    pub vm_names: &'a [String],
    /// The run's own key Secret, which stays for the whole run.
    pub run_secret: String,
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

        if let Some(v) = self.json(&format!("{}/api/v1/slabs", self.stormblock)).await {
            let (bytes, slots) = slab_allocated(&v);
            c.slab_allocated_bytes = Some(bytes);
            c.slab_allocated_slots = Some(slots);
        }

        if let Ok(r) = self.kube.get(&format!("/api/v1/namespaces/{}/secrets", self.namespace)).await
            && r.ok()
        {
            c.own.secrets = kube::items(&r.body)
                .iter()
                .filter(|s| s["type"] != "kubernetes.io/service-account-token")
                .filter_map(|s| s["metadata"]["name"].as_str())
                .filter(|n| *n != self.run_secret.as_str())
                .map(str::to_string)
                .collect();
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

        // Only the host's network namespace shows taps and veths; a pod's
        // own shows lo and eth0, which would read as a wrong zero.
        if let Ok(dev) = tokio::fs::read_to_string(format!("{}/net/dev", self.proc_root)).await
            && host_netns(&dev, self.bridge)
        {
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
        assert!(is_engine(b"/usr/bin/stormblock\0adopt-ublk\0--api\00.0.0.0:9090\0"));
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

    #[test]
    fn memory() {
        let m = "MemTotal:       1000 kB\nMemFree: 1 kB\nMemAvailable:    400 kB\n";
        assert_eq!(mem_used(m), Some(600 * 1024));
        assert_eq!(mem_available(m), Some(400 * 1024));
    }
}
