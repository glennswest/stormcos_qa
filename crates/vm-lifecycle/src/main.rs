//! vm-lifecycle — the VM lifecycle soak, run as overnight **waves**
//! (stormcos_qa#16; stormcentral docs/test-standard.md, "Overnight soaks").
//!
//! Each wave ramps VMs to a size taken from the machine's own capacity (10 is
//! the smallest, ~80% of allocatable memory the largest), holds them — each is
//! Running with an address, answers **ssh** with the run's key and **RDP**
//! through stormrdp, installs a package, is **restarted** and still has the
//! package — then drains them and checks nothing of the run is left. It
//! repeats with varying sizes until the window (`STORM_TIMEOUT`) or `--waves`
//! runs out.
//!
//! Across waves it measures start latency and residue (node memory,
//! stormblock volumes and attachments, stormvm registrations, taps, file
//! handles). A wave slower than the first, or a residue that grows, fails even
//! when every operation in it passed.
//!
//! Output: JSON lines on stdout and in `<results>/vm-lifecycle.jsonl`, the
//! trend in `<results>/waves.json`, failure evidence per VM beside them.
//! Exit 0 all passed (or skipped), 1 something failed, 2 could not run.

mod census;
mod kube;
mod rdp;
mod report;
mod ssh;
mod wave;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use serde_json::{Value, json};

use report::{Line, Out, Status};

#[derive(Parser, Debug)]
#[command(name = "vm-lifecycle", version, about)]
pub struct Args {
    /// Apiserver URL. Empty: in-cluster (service account).
    #[arg(long, env = "STORM_API", default_value = "")]
    api: String,
    /// Bearer token file (default: the service account's).
    #[arg(long)]
    token_file: Option<String>,
    /// Skip TLS verification of the apiserver (outside a cluster).
    #[arg(long)]
    insecure: bool,
    /// The run's own namespace; everything is created in it.
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    /// Labels everything as storm.io/test-run=<id>.
    #[arg(long, env = "STORM_RUN_ID")]
    run_id: Option<String>,
    /// The node under test: its address or name.
    #[arg(long, env = "STORM_NODE", default_value = "127.0.0.1")]
    node: String,
    /// Seconds the whole run may take (the night window for `long`).
    #[arg(long, env = "STORM_TIMEOUT", default_value_t = 28800)]
    timeout: u64,
    /// Stop after this many waves (0: until the window ends).
    #[arg(long, visible_alias = "cycles", default_value_t = 0)]
    waves: usize,
    /// Every wave this many VMs (0: sized from the machine's capacity).
    #[arg(long, default_value_t = 0)]
    vms: usize,
    /// The smallest wave. A machine that cannot hold it reports skip.
    #[arg(long, default_value_t = 10)]
    min_vms: usize,
    /// Largest wave as a fraction of the node's allocatable memory.
    #[arg(long, default_value_t = 0.8)]
    capacity_fraction: f64,
    #[arg(long, default_value_t = 2048)]
    vm_memory_mib: u64,
    #[arg(long, default_value_t = 1)]
    vm_cores: u32,
    /// The Linux golden each VM's root disk is cloned from.
    #[arg(long, default_value = "fedora-44-x86_64")]
    golden: String,
    /// The golden's cloud user.
    #[arg(long, default_value = "fedora")]
    ssh_user: String,
    /// Installed with dnf or apt-get, then checked across the restart.
    #[arg(long, default_value = "jq")]
    package: String,
    /// Host bridge the VMs' NIC is put on (`storm.io/bridge`).
    #[arg(long, default_value = "stormbr0")]
    bridge: String,
    /// stormblock's API (default http://<node>:9090).
    #[arg(long)]
    stormblock_url: Option<String>,
    /// stormvm's API (loopback-only on stormcos; the Job is hostNetwork).
    #[arg(long, default_value = "http://127.0.0.1:9095")]
    stormvm_url: String,
    /// stormrdp's gateway (default <node>:3389).
    #[arg(long)]
    rdp: Option<String>,
    /// The host's /proc (taps, memory, file handles).
    #[arg(long, default_value = "/proc")]
    proc_root: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: PathBuf,
    /// Per VM: create → Running, address, ssh and RDP.
    #[arg(long, default_value_t = 900)]
    ready_timeout: u64,
    #[arg(long, default_value_t = 600)]
    install_timeout: u64,
    /// After deleting a wave: until all of it is gone from API and node.
    #[arg(long, default_value_t = 300)]
    drain_timeout: u64,
    /// A wave's median start latency may be this × the first wave's…
    #[arg(long, default_value_t = 1.5)]
    slowdown: f64,
    /// …plus this many seconds.
    #[arg(long, default_value_t = 30)]
    slowdown_grace_secs: u64,
    /// Node memory in use after a drain may exceed the baseline by this much.
    #[arg(long, default_value_t = 512)]
    mem_slack_mib: u64,
    /// Allocated file handles after a drain may exceed the baseline by this.
    #[arg(long, default_value_t = 2048)]
    fd_slack: u64,
    /// Also put the key in the cloud-init user-data. A diagnostic bypass
    /// while stormvm#41 (accessCredentials) is open — not a pass of #16.
    #[arg(long)]
    seed_key: bool,
}

impl Args {
    fn rdp_addr(&self) -> String {
        self.rdp.clone().unwrap_or_else(|| format!("{}:3389", self.node))
    }
    fn stormblock(&self) -> String {
        self.stormblock_url.clone().unwrap_or_else(|| format!("http://{}:9090", self.node)).trim_end_matches('/').to_string()
    }
}

pub struct Ctx {
    pub args: Args,
    pub kube: kube::Client,
    pub http: reqwest::Client,
    pub key: ssh::Key,
    pub namespace: String,
    pub run_id: String,
    pub node_name: Option<String>,
    pub results: PathBuf,
    pub out: Out,
    stormblock: String,
    names: Vec<String>,
}

impl Ctx {
    pub fn secret_name(&self) -> String {
        format!("{}-ssh", self.prefix())
    }
    fn prefix(&self) -> String {
        let id: String = self.run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect();
        format!("vl{}", id.to_ascii_lowercase())
    }
    pub fn sources(&self) -> census::Sources<'_> {
        census::Sources {
            kube: &self.kube,
            http: &self.http,
            namespace: &self.namespace,
            stormblock: &self.stormblock,
            stormvm: self.args.stormvm_url.trim_end_matches('/'),
            proc_root: &self.args.proc_root,
            vm_names: &self.names,
        }
    }
}

/// Could not run: exit 2.
struct Infra(String);

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let out = Out::new(&args.results);
    let code = match run(args, out).await {
        Ok(code) => code,
        Err(e) => {
            // `run` has already reported; anything reaching here is infra.
            eprintln!("vm-lifecycle: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

async fn run(args: Args, out: Out) -> Result<i32> {
    let started = Instant::now();
    let kube = kube::Client::new(&args.api, args.token_file.as_deref(), args.insecure).await?;
    let namespace = match &args.namespace {
        Some(n) => n.clone(),
        None => tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
            .await
            .context("no --namespace / STORM_NAMESPACE and no service-account namespace")?,
    };
    let run_id = args.run_id.clone().unwrap_or_else(|| {
        let mut b = [0u8; 4];
        let _ = getrandom::getrandom(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    });
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let results = args.results.clone();
    let stormblock = args.stormblock();
    let mut ctx = Ctx {
        key: ssh::Key::generate()?,
        kube,
        http,
        namespace,
        run_id,
        node_name: None,
        results,
        out,
        stormblock,
        names: Vec::new(),
        args,
    };

    let (max, cap_detail) = match preflight(&mut ctx).await {
        Ok(Ok(v)) => v,
        Ok(Err(skip)) => {
            ctx.out.emit(Line::new("vm-lifecycle", Status::Skip, started.elapsed(), skip));
            ctx.out.summary();
            return Ok(0);
        }
        Err(Infra(why)) => {
            ctx.out.emit(Line::new("vm-lifecycle/preflight", Status::Fail, started.elapsed(), why));
            ctx.out.summary();
            return Ok(2);
        }
    };
    let min = if ctx.args.vms > 0 { ctx.args.vms } else { ctx.args.min_vms };
    let max = if ctx.args.vms > 0 { ctx.args.vms } else { max };
    ctx.names = (1..=max).map(|i| format!("{}-{i:03}", ctx.prefix())).collect();
    ctx.out.emit(Line::new(
        "vm-lifecycle/preflight",
        Status::Pass,
        started.elapsed(),
        format!("namespace {}, run {}, node {:?}, waves {min}..{max} VMs ({cap_detail})", ctx.namespace, ctx.run_id, ctx.node_name),
    ));

    let ctx = Arc::new(ctx);
    let code = soak(ctx.clone(), started, min, max).await;
    cleanup(&ctx).await;
    ctx.out.summary();
    Ok(code)
}

/// `Ok(Ok((largest wave, how it was sized)))`, `Ok(Err(skip reason))` or
/// `Err(Infra)`.
async fn preflight(ctx: &mut Ctx) -> Result<Result<(usize, String), String>, Infra> {
    let infra = |e: anyhow::Error| Infra(format!("{e:#}"));
    let r = ctx.kube.get(&kube::vms(&ctx.namespace)).await.map_err(infra)?;
    match r.code {
        200 => {}
        404 => return Err(Infra("the VirtualMachine resource is not served (kubevirt.io/v1 CRD missing)".into())),
        c => return Err(Infra(format!("listing VirtualMachines in {} answered {c}: {}", ctx.namespace, r.body))),
    }

    let r = ctx.kube.get("/api/v1/nodes").await.map_err(infra)?;
    if !r.ok() {
        return Err(Infra(format!("listing nodes answered {}: {}", r.code, r.body)));
    }
    let nodes = kube::items(&r.body);
    let node = nodes
        .iter()
        .find(|n| {
            n["metadata"]["name"].as_str() == Some(&ctx.args.node)
                || n["status"]["addresses"].as_array().into_iter().flatten().any(|a| a["address"].as_str() == Some(&ctx.args.node))
        })
        .or(if nodes.len() == 1 { nodes.first() } else { None })
        .ok_or_else(|| Infra(format!("{} nodes and none is {:?}: cannot tell which is under test", nodes.len(), ctx.args.node)))?;
    ctx.node_name = node["metadata"]["name"].as_str().map(str::to_string);
    let alloc = node["status"]["allocatable"]["memory"]
        .as_str()
        .and_then(kube::quantity_bytes)
        .ok_or_else(|| Infra(format!("node {:?} reports no allocatable memory", ctx.node_name)))?;

    match ctx.http.get(format!("{}/api/v1/volumes/{}", ctx.stormblock, ctx.args.golden)).send().await {
        Ok(r) if r.status().as_u16() == 404 => {
            return Err(Infra(format!("golden {} is not on the node's stormblock", ctx.args.golden)));
        }
        Ok(_) => {}
        Err(e) => eprintln!("vm-lifecycle: stormblock at {} unreachable ({e}); volume residue unmeasured", ctx.stormblock),
    }

    // The run's key, for accessCredentials.
    let secret = json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": { "name": ctx.secret_name(), "labels": { "storm.io/test-run": ctx.run_id } },
        "stringData": { "key": ctx.key.public_openssh }
    });
    let path = format!("/api/v1/namespaces/{}/secrets", ctx.namespace);
    let _ = ctx.kube.delete(&format!("{path}/{}", ctx.secret_name())).await;
    let r = ctx.kube.post(&path, &secret).await.map_err(infra)?;
    if !r.ok() {
        return Err(Infra(format!("creating the key Secret answered {}: {}", r.code, r.body)));
    }

    let vm = ctx.args.vm_memory_mib << 20;
    let by_alloc = ((alloc as f64 * ctx.args.capacity_fraction) / vm as f64) as usize;
    let avail = tokio::fs::read_to_string(format!("{}/meminfo", ctx.args.proc_root)).await.ok().and_then(|m| census::mem_available(&m));
    // Guest memory plus ~10% for qemu, keeping 1 GiB for the node itself.
    let by_avail = avail.map(|a| (a.saturating_sub(1 << 30) as f64 / (vm as f64 * 1.1)) as usize);
    let max = by_avail.map_or(by_alloc, |b| b.min(by_alloc));
    let detail = format!(
        "allocatable {} MiB × {} / {} MiB per VM = {by_alloc}; MemAvailable allows {by_avail:?}",
        alloc >> 20,
        ctx.args.capacity_fraction,
        ctx.args.vm_memory_mib
    );
    let need = if ctx.args.vms > 0 { ctx.args.vms } else { ctx.args.min_vms };
    if max < need {
        return Ok(Err(format!("requires memory for {need} VMs: this machine holds {max} ({detail})")));
    }
    Ok(Ok((max, detail)))
}

/// Wave `k`'s size: the first is the smallest (the latency baseline), then
/// it varies across the range.
fn wave_size(k: usize, min: usize, max: usize) -> usize {
    const MIX: [f64; 7] = [0.0, 1.0, 0.5, 0.0, 0.75, 1.0, 0.25];
    min + ((max - min) as f64 * MIX[k % MIX.len()]).round() as usize
}

#[derive(Debug, Clone, Serialize)]
struct WaveRecord {
    wave: usize,
    vms: usize,
    ms: u64,
    failed_vms: usize,
    start_ms_median: Option<u64>,
    start_ms_max: Option<u64>,
    restart_ms_median: Option<u64>,
    residue: census::Census,
    left_behind: census::Own,
    regressions: Vec<String>,
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied().filter(|_| !v.is_empty())
}

/// Metrics that must not grow across drains: (name, value, slack).
fn growth_metrics(c: &census::Census, a: &Args) -> Vec<(&'static str, Option<u64>, u64)> {
    vec![
        ("stormblock volumes", c.volumes.map(|v| v as u64), 0),
        ("stormblock attachments", c.attachments.map(|v| v as u64), 0),
        ("stormvm registrations", c.registrations.map(|v| v as u64), 0),
        ("taps", c.taps.map(|v| v as u64), 0),
        ("node memory in use (bytes)", c.mem_used_bytes, a.mem_slack_mib << 20),
        ("allocated file handles", c.fds, a.fd_slack),
    ]
}

/// A metric regressed when, after a drain, it is above the baseline by more
/// than its slack **and** above the previous drain: still growing, not a
/// plateau from something warming up once.
fn regressions(base: &census::Census, prev: Option<&census::Census>, now: &census::Census, a: &Args) -> Vec<String> {
    let b = growth_metrics(base, a);
    let p = prev.map(|p| growth_metrics(p, a));
    growth_metrics(now, a)
        .into_iter()
        .enumerate()
        .filter_map(|(i, (name, v, slack))| {
            let (v, b0) = (v?, b[i].1?);
            let prev_v = p.as_ref().and_then(|p| p[i].1);
            (v > b0 + slack && prev_v.is_none_or(|pv| v > pv)).then(|| format!("{name}: {b0} before the first wave, {v} now"))
        })
        .collect()
}

async fn soak(ctx: Arc<Ctx>, started: Instant, min: usize, max: usize) -> i32 {
    let reserve = Duration::from_secs(ctx.args.drain_timeout + 60);
    let window_end = started + Duration::from_secs(ctx.args.timeout).saturating_sub(reserve);
    let baseline = ctx.sources().take().await;
    ctx.out.emit(Line {
        wave: Some(json!(baseline)),
        ..Line::new("baseline", Status::Pass, started.elapsed(), "node census before the first wave")
    });

    let mut records: Vec<WaveRecord> = Vec::new();
    for k in 0.. {
        if ctx.args.waves > 0 && k >= ctx.args.waves {
            break;
        }
        let size = wave_size(k, min, max);
        if let Some(last) = records.last() {
            let estimate = Duration::from_millis(last.ms).mul_f64((size as f64 / last.vms as f64).max(1.0) * 1.2);
            if Instant::now() + estimate > window_end {
                break;
            }
        }
        let rec = run_wave(&ctx, k + 1, size, window_end, &baseline, &records).await;
        records.push(rec);
        let _ = tokio::fs::write(ctx.results.join("waves.json"), serde_json::to_vec_pretty(&records).unwrap_or_default()).await;
    }
    if records.is_empty() {
        ctx.out.emit(Line::new("vm-lifecycle", Status::Fail, started.elapsed(), "the window closed before a single wave"));
    }
    if ctx.out.failed() > 0 { 1 } else { 0 }
}

async fn run_wave(ctx: &Arc<Ctx>, wave: usize, size: usize, window_end: Instant, baseline: &census::Census, prior: &[WaveRecord]) -> WaveRecord {
    let t0 = Instant::now();
    let names: Vec<String> = ctx.names[..size].to_vec();

    // Ramp + hold, every VM concurrently.
    let mut set = tokio::task::JoinSet::new();
    for vm in names.iter().cloned() {
        let ctx = ctx.clone();
        set.spawn(async move {
            let created = Instant::now();
            let r = ctx.kube.post(&kube::vms(&ctx.namespace), &wave::manifest(&ctx, wave, &vm)).await;
            match r {
                Ok(r) if r.ok() => wave::hold(ctx, wave, vm, created).await,
                other => {
                    let why = match other {
                        Ok(r) => format!("create answered {}: {}", r.code, r.body),
                        Err(e) => format!("{e:#}"),
                    };
                    ctx.out.emit(Line::new(format!("wave-{wave}/{vm}/create"), Status::Fail, created.elapsed(), why));
                    wave::VmResult { vm, failed: true, ..Default::default() }
                }
            }
        });
    }
    let mut vms = Vec::new();
    let remaining = window_end.saturating_duration_since(Instant::now()).max(Duration::from_secs(60));
    let collected = tokio::time::timeout(remaining, async {
        while let Some(r) = set.join_next().await {
            if let Ok(r) = r {
                vms.push(r);
            }
        }
    })
    .await;
    if collected.is_err() {
        set.abort_all();
        ctx.out.emit(Line::new(format!("wave-{wave}/hold"), Status::Fail, t0.elapsed(), "the window closed mid-wave"));
    }

    // Drain.
    let d0 = Instant::now();
    let left = wave::drain(ctx, &names).await.unwrap_or_default();
    if left.is_empty() {
        ctx.out.emit(Line::new(format!("wave-{wave}/drain"), Status::Pass, d0.elapsed(), format!("{size} VMs deleted, nothing of the run left")));
    } else {
        ctx.out.emit(Line::new(format!("wave-{wave}/drain"), Status::Fail, d0.elapsed(), format!("left behind after {}s: {}", ctx.args.drain_timeout, json!(left))));
    }
    let residue = ctx.sources().take().await;

    let start_ms_median = median(vms.iter().filter_map(|v| v.start_ms).collect());
    let mut regress = regressions(baseline, prior.last().map(|p| &p.residue), &residue, &ctx.args);
    if let (Some(first), Some(now)) = (prior.first().and_then(|f| f.start_ms_median), start_ms_median) {
        let limit = (first as f64 * ctx.args.slowdown) as u64 + ctx.args.slowdown_grace_secs * 1000;
        if now > limit {
            regress.push(format!("median start {now} ms vs {first} ms in wave 1 (limit {limit} ms)"));
        }
    }
    let rec = WaveRecord {
        wave,
        vms: size,
        ms: t0.elapsed().as_millis() as u64,
        failed_vms: vms.iter().filter(|v| v.failed).count() + size.saturating_sub(vms.len()),
        start_ms_median,
        start_ms_max: vms.iter().filter_map(|v| v.start_ms).max(),
        restart_ms_median: median(vms.iter().filter_map(|v| v.restart_ms).collect()),
        residue,
        left_behind: left,
        regressions: regress,
    };
    let ok = rec.failed_vms == 0 && rec.left_behind.is_empty() && rec.regressions.is_empty();
    let detail = if rec.regressions.is_empty() {
        format!("{} VMs, {} failed", size, rec.failed_vms)
    } else {
        format!("{} VMs, {} failed; regressed: {}", size, rec.failed_vms, rec.regressions.join("; "))
    };
    ctx.out.emit(Line {
        wave: Some(serde_json::to_value(&rec).unwrap_or(Value::Null)),
        ..Line::new(format!("wave-{wave}"), if ok { Status::Pass } else { Status::Fail }, t0.elapsed(), detail)
    });
    rec
}

/// Leave the host as found: the run's VMs and key Secret. The namespace
/// itself is the runner's to delete.
async fn cleanup(ctx: &Ctx) {
    if let Ok(r) = ctx.kube.get(&format!("{}?labelSelector=storm.io/test-run%3D{}", kube::vms(&ctx.namespace), ctx.run_id)).await {
        for vm in kube::items(&r.body) {
            if let Some(n) = vm["metadata"]["name"].as_str() {
                let _ = ctx.kube.delete(&format!("{}/{n}", kube::vms(&ctx.namespace))).await;
            }
        }
    }
    let _ = ctx.kube.delete(&format!("/api/v1/namespaces/{}/secrets/{}", ctx.namespace, ctx.secret_name())).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waves_start_smallest_and_stay_in_range() {
        assert_eq!(wave_size(0, 10, 40), 10);
        assert_eq!(wave_size(1, 10, 40), 40);
        for k in 0..30 {
            let s = wave_size(k, 10, 40);
            assert!((10..=40).contains(&s));
        }
        assert_eq!(wave_size(5, 10, 10), 10);
    }

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3, 1, 2]), Some(2));
    }

    #[test]
    fn growth_is_a_regression_a_plateau_is_not() {
        let a = Args::parse_from(["vm-lifecycle"]);
        let c = |v| census::Census { volumes: Some(v), ..Default::default() };
        assert!(regressions(&c(5), None, &c(5), &a).is_empty());
        assert_eq!(regressions(&c(5), None, &c(6), &a).len(), 1);
        assert_eq!(regressions(&c(5), Some(&c(6)), &c(7), &a).len(), 1);
        assert!(regressions(&c(5), Some(&c(7)), &c(7), &a).is_empty());
        // Unmeasured is never a regression, and never a silent zero.
        assert!(regressions(&census::Census::default(), None, &c(9), &a).is_empty());
    }
}
