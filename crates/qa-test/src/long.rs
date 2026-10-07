//! `/test long` — the overnight soak, run as **waves** of two kinds
//! (stormcentral docs/test-standard.md, "Overnight soaks": "every night runs
//! both kinds"), alternating:
//!
//! - **containers** (#17, `containers.rs`): Deployments up to the node's free
//!   pod slots (10 is the smallest wave, ~80% of what is free the largest),
//!   each pod with its own stormblock claim, a readiness probe and a Service
//!   in front. Hold: every pod Ready behind the Service, restarted in place,
//!   then rescheduled, each time reading back what it wrote to its claim;
//! - **VMs** (#16, `wave.rs`): VMs up to ~80% of allocatable memory. Each is
//!   Running with an address, answers **ssh** with the run's key and **RDP**
//!   through stormrdp, installs a package, is **restarted** and still has it.
//!
//! Each wave is drained, and nothing of it may be left. Waves repeat with
//! varying sizes until the window (`STORM_TIMEOUT`) or `--waves` runs out.
//! Each kind is checked on its own first: a machine without what VMs need
//! (the golden, the VM resource, the memory for 10) still runs the container
//! waves.
//!
//! Across waves it measures start latency (per kind, against that kind's
//! first wave) and residue (node memory, stormblock volumes and attachments,
//! stormvm registrations, taps, veths, cgroups, PVs, file handles). A wave
//! slower than the first of its kind, or a residue that grows, fails even
//! when every operation in it passed.
//!
//! Output: JSON lines on stdout and in `<results>/long.jsonl`, the trend in
//! `<results>/waves.json`, failure evidence beside them. Exit 0 all passed
//! (or skipped), 1 something failed, 2 could not run (a kind that could not
//! run, and nothing failed).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};

use crate::report::{Line, Out, Status};
use crate::{census, containers, kube, ssh, wave};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Containers,
    Vms,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Containers => "containers",
            Kind::Vms => "vms",
        }
    }
}

#[derive(Parser, Debug)]
// A flag given again wins over the earlier one: `container-waves` puts its
// preset (`CONTAINER_WAVES`, `VM_WAVES`) before the caller's flags.
#[command(args_override_self = true)]
#[command(name = "test long", about = "The overnight soak: container waves (#17) and VM waves (#16), alternating")]
pub struct Args {
    /// Apiserver URL. Empty: in-cluster (service account).
    #[arg(long, env = "STORM_API", default_value = "")]
    pub(crate) api: String,
    /// Bearer token file (default: the service account's).
    #[arg(long)]
    pub(crate) token_file: Option<String>,
    /// Skip TLS verification of the apiserver (outside a cluster).
    #[arg(long)]
    pub(crate) insecure: bool,
    /// The run's own namespace; everything is created in it.
    #[arg(long, env = "STORM_NAMESPACE")]
    pub(crate) namespace: Option<String>,
    /// Labels everything as storm.io/test-run=<id>.
    #[arg(long, env = "STORM_RUN_ID")]
    pub(crate) run_id: Option<String>,
    /// The node under test: its address or name.
    #[arg(long, env = "STORM_NODE", default_value = "127.0.0.1")]
    pub(crate) node: String,
    /// Seconds the whole run may take (the night window for `long`).
    #[arg(long, env = "STORM_TIMEOUT", default_value_t = 28800)]
    pub(crate) timeout: u64,
    /// Stop after this many waves, of all kinds (0: until the window ends).
    #[arg(long, visible_alias = "cycles", default_value_t = 0)]
    pub(crate) waves: usize,
    /// The kinds of wave, taken in turn. Given again, the last list wins
    /// (`Set`, not clap's default append for a list).
    #[arg(long, value_enum, value_delimiter = ',', action = clap::ArgAction::Set, default_value = "containers,vms")]
    pub(crate) kinds: Vec<Kind>,

    // ---- VM waves (#16) ----
    /// Every VM wave this many VMs (0: sized from the machine's capacity).
    #[arg(long, default_value_t = 0)]
    pub(crate) vms: usize,
    /// The smallest VM wave. A machine that cannot hold it skips VM waves.
    #[arg(long, default_value_t = 10)]
    pub(crate) min_vms: usize,
    /// …and at most this many VMs (0: no cap).
    #[arg(long, default_value_t = 0)]
    pub(crate) max_vms: usize,
    /// Largest VM wave as a fraction of the node's allocatable memory.
    #[arg(long, default_value_t = 0.8)]
    pub(crate) capacity_fraction: f64,
    #[arg(long, default_value_t = 2048)]
    pub(crate) vm_memory_mib: u64,
    #[arg(long, default_value_t = 1)]
    pub(crate) vm_cores: u32,
    /// The Linux golden each VM's root disk is cloned from.
    #[arg(long, default_value = "fedora-44-x86_64")]
    pub(crate) golden: String,
    /// The golden's cloud user.
    #[arg(long, default_value = "fedora")]
    pub(crate) ssh_user: String,
    /// Installed with dnf or apt-get, then checked across the restart.
    #[arg(long, default_value = "jq")]
    pub(crate) package: String,
    /// Host bridge the VMs' NIC is put on (`storm.io/bridge`).
    #[arg(long, default_value = "stormbr0")]
    pub(crate) bridge: String,
    /// stormvm's API (loopback-only on stormcos; the Job is hostNetwork).
    #[arg(long, default_value = "http://127.0.0.1:9095")]
    pub(crate) stormvm_url: String,
    /// stormrdp's gateway (default <node>:3389).
    #[arg(long)]
    pub(crate) rdp: Option<String>,
    #[arg(long, default_value_t = 600)]
    pub(crate) install_timeout: u64,
    /// Also put the key in the cloud-init user-data. A diagnostic bypass
    /// while stormvm#41 (accessCredentials) is open — not a pass of #16.
    #[arg(long)]
    pub(crate) seed_key: bool,

    // ---- container waves (#17) ----
    /// Every container wave this many pods (0: sized from free pod slots).
    #[arg(long, default_value_t = 0)]
    pub(crate) pods: usize,
    /// The smallest container wave. A node with fewer free slots skips them.
    #[arg(long, default_value_t = 10)]
    pub(crate) min_pods: usize,
    /// Largest container wave as a fraction of the node's free pod slots.
    #[arg(long, default_value_t = 0.8)]
    pub(crate) pod_fraction: f64,
    /// …and at most this many pods (0: no cap).
    #[arg(long, default_value_t = 0)]
    pub(crate) max_pods: usize,
    /// Each pod's claim.
    #[arg(long, default_value_t = 64)]
    pub(crate) claim_size_mib: u64,
    /// The claims' StorageClass (default: the cluster's default class).
    #[arg(long)]
    pub(crate) storage_class: Option<String>,
    #[arg(long, default_value_t = 16)]
    pub(crate) pod_memory_mib: u64,
    /// Each pod's container exits once, this long after it starts, to be
    /// restarted in place.
    #[arg(long, default_value_t = 30)]
    pub(crate) restart_after: u64,
    /// This test's image, run by the wave's pods as `/test claim`
    /// (default: the Job pod's own).
    #[arg(long)]
    pub(crate) image: Option<String>,

    // ---- both ----
    /// stormblock's API (default http://<node>:9090).
    #[arg(long)]
    pub(crate) stormblock_url: Option<String>,
    /// The host's /proc (taps, veths, cgroups, memory, file handles).
    #[arg(long, default_value = "/proc")]
    pub(crate) proc_root: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    pub(crate) results: PathBuf,
    /// Per VM or pod: create → up (VM: Running, address, ssh and RDP; pod: Ready).
    #[arg(long, default_value_t = 900)]
    pub(crate) ready_timeout: u64,
    /// After deleting a wave: until all of it is gone from API and node.
    #[arg(long, default_value_t = 300)]
    pub(crate) drain_timeout: u64,
    /// A wave's median start latency may be this × its kind's first wave's…
    #[arg(long, default_value_t = 1.5)]
    pub(crate) slowdown: f64,
    /// …plus this many seconds.
    #[arg(long, default_value_t = 30)]
    pub(crate) slowdown_grace_secs: u64,
    /// Node memory in use after a drain may exceed the baseline by this much.
    #[arg(long, default_value_t = 512)]
    pub(crate) mem_slack_mib: u64,
    /// Allocated file handles after a drain may exceed the baseline by this.
    #[arg(long, default_value_t = 2048)]
    pub(crate) fd_slack: u64,
    /// cgroups after a drain may exceed the baseline by this many.
    #[arg(long, default_value_t = 16)]
    pub(crate) cgroup_slack: u64,
}

impl Args {
    pub(crate) fn rdp_addr(&self) -> String {
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
    /// stormblock's: sends the engine token (stormblock#107, #19).
    pub stormblock_http: reqwest::Client,
    /// Whether a stormblock token was found.
    pub stormblock_token: bool,
    pub key: ssh::Key,
    pub namespace: String,
    pub run_id: String,
    pub node_name: Option<String>,
    /// The node's `kubernetes.io/hostname` label (its name if unlabelled):
    /// VMs and pods are pinned with a nodeSelector on it, through the
    /// scheduler.
    pub node_hostname: Option<String>,
    pub results: PathBuf,
    pub out: Out,
    stormblock: String,
    /// VM names (VM waves reuse them).
    names: Vec<String>,
    /// Fail lines that mean "could not run", not "failed".
    infra: AtomicUsize,
}

impl Ctx {
    pub fn secret_name(&self) -> String {
        format!("{}-ssh", self.prefix())
    }
    fn short_id(&self) -> String {
        let id: String = self.run_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect();
        id.to_ascii_lowercase()
    }
    fn prefix(&self) -> String {
        format!("vl{}", self.short_id())
    }
    /// Container waves' names: `ct<id>-w<wave>-<n>`.
    pub fn prefix_containers(&self) -> String {
        format!("ct{}", self.short_id())
    }
    pub fn sources(&self) -> census::Sources<'_> {
        census::Sources {
            kube: &self.kube,
            http: &self.http,
            stormblock_http: &self.stormblock_http,
            namespace: &self.namespace,
            stormblock: &self.stormblock,
            stormvm: self.args.stormvm_url.trim_end_matches('/'),
            proc_root: &self.args.proc_root,
            bridge: &self.args.bridge,
            vm_names: &self.names,
        }
    }
    fn could_not_run(&self, test: impl Into<String>, took: Duration, why: impl Into<String>) {
        self.infra.fetch_add(1, Ordering::Relaxed);
        self.out.emit(Line::new(test, Status::Fail, took, format!("could not run: {}", why.into())));
    }
    /// 1 on a real failure; else 2 when a part could not run; else 0.
    fn exit_code(&self) -> i32 {
        let infra = self.infra.load(Ordering::Relaxed);
        if self.out.failed() > infra {
            1
        } else if infra > 0 {
            2
        } else {
            0
        }
    }
}

/// Could not run.
struct Infra(String);

pub async fn main(args: Args) -> i32 {
    let out = Out::new(&args.results, "long");
    match run(args, out).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("test long: {e:#}");
            2
        }
    }
}

/// A kind that can run, with its wave range.
struct Plan {
    kind: Kind,
    min: usize,
    max: usize,
    image: Option<String>,
}

async fn run(args: Args, out: Out) -> Result<i32> {
    let started = Instant::now();
    let kube = kube::Client::new(&args.api, args.token_file.as_deref(), args.insecure).await?;
    let namespace = match &args.namespace {
        Some(n) => n.clone(),
        None => tokio::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
            .await
            .context("no --namespace / STORM_NAMESPACE and no service-account namespace")?
            .trim()
            .to_string(),
    };
    let run_id = args.run_id.clone().unwrap_or_else(|| {
        let mut b = [0u8; 4];
        let _ = getrandom::getrandom(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    });
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let results = args.results.clone();
    let stormblock = args.stormblock();
    let sb_token = census::stormblock_token();
    let stormblock_http = census::stormblock_client(sb_token.as_deref())?;
    let mut ctx = Ctx {
        key: ssh::Key::generate()?,
        kube,
        http,
        stormblock_http,
        stormblock_token: sb_token.is_some(),
        namespace,
        run_id,
        node_name: None,
        node_hostname: None,
        results,
        out,
        stormblock,
        names: Vec::new(),
        infra: AtomicUsize::new(0),
        args,
    };

    let node = match node_preflight(&mut ctx).await {
        Ok(n) => n,
        Err(Infra(why)) => {
            ctx.could_not_run("long/preflight", started.elapsed(), why);
            ctx.out.summary();
            return Ok(2);
        }
    };

    let mut plans = Vec::new();
    let mut kinds = ctx.args.kinds.clone();
    kinds.dedup();
    for kind in kinds {
        let t = Instant::now();
        let test = format!("{}/preflight", kind.name());
        let r = match kind {
            Kind::Vms => vm_preflight(&mut ctx, &node).await,
            Kind::Containers => container_preflight(&ctx, &node).await,
        };
        match r {
            Ok(Ok(p)) => {
                ctx.out.emit(Line::new(
                    test,
                    Status::Pass,
                    t.elapsed(),
                    format!("namespace {}, run {}, node {:?}, waves {}..{} ({})", ctx.namespace, ctx.run_id, ctx.node_name, p.min, p.max, p.detail),
                ));
                plans.push(Plan { kind, min: p.min, max: p.max, image: p.image });
            }
            Ok(Err(skip)) => ctx.out.emit(Line::new(test, Status::Skip, t.elapsed(), skip)),
            Err(Infra(why)) => ctx.could_not_run(test, t.elapsed(), why),
        }
    }
    if let Some(p) = plans.iter().find(|p| p.kind == Kind::Vms) {
        ctx.names = (1..=p.max).map(|i| format!("{}-{i:03}", ctx.prefix())).collect();
    }

    let ctx = Arc::new(ctx);
    if !plans.is_empty() {
        soak(ctx.clone(), started, &plans).await;
    }
    cleanup(&ctx).await;
    ctx.out.summary();
    Ok(ctx.exit_code())
}

/// The node under test, from the API: `(the Node, its allocatable memory)`.
async fn node_preflight(ctx: &mut Ctx) -> Result<Value, Infra> {
    let infra = |e: anyhow::Error| Infra(format!("{e:#}"));
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
        .ok_or_else(|| Infra(format!("{} nodes and none is {:?}: cannot tell which is under test", nodes.len(), ctx.args.node)))?
        .clone();
    ctx.node_name = node["metadata"]["name"].as_str().map(str::to_string);
    ctx.node_hostname = hostname_label(&node).or_else(|| ctx.node_name.clone());
    Ok(node)
}

struct KindPlan {
    min: usize,
    max: usize,
    detail: String,
    image: Option<String>,
}

/// `Ok(Ok(plan))`, `Ok(Err(skip reason))` or `Err(Infra)`.
async fn vm_preflight(ctx: &mut Ctx, node: &Value) -> Result<Result<KindPlan, String>, Infra> {
    let infra = |e: anyhow::Error| Infra(format!("{e:#}"));
    let r = ctx.kube.get(&kube::vms(&ctx.namespace)).await.map_err(infra)?;
    match r.code {
        200 => {}
        404 => return Err(Infra("the VirtualMachine resource is not served (kubevirt.io/v1 CRD missing)".into())),
        c => return Err(Infra(format!("listing VirtualMachines in {} answered {c}: {}", ctx.namespace, r.body))),
    }
    let alloc = node["status"]["allocatable"]["memory"]
        .as_str()
        .and_then(kube::quantity_bytes)
        .ok_or_else(|| Infra(format!("node {:?} reports no allocatable memory", ctx.node_name)))?;

    // Only a 2xx says the golden is here. A 401 used to count as present, so a
    // run without the token went on to wait 15 minutes for VMs that could not
    // start.
    match ctx.stormblock_http.get(format!("{}/api/v1/volumes/{}", ctx.stormblock, ctx.args.golden)).send().await {
        Ok(r) if r.status().as_u16() == 404 => {
            return Err(Infra(format!("golden {} is not on the node's stormblock", ctx.args.golden)));
        }
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!(
            "long: stormblock answered {} for golden {} ({}); golden and volume residue unverified",
            r.status().as_u16(),
            ctx.args.golden,
            if ctx.stormblock_token { "token sent" } else { "no token: set STORMBLOCK_API_TOKEN or STORMBLOCK_TOKEN_FILE" },
        ),
        Err(e) => eprintln!("long: stormblock at {} unreachable ({e}); volume residue unmeasured", ctx.stormblock),
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
        "VMs: allocatable {} MiB × {} / {} MiB per VM = {by_alloc}; MemAvailable allows {by_avail:?}",
        alloc >> 20,
        ctx.args.capacity_fraction,
        ctx.args.vm_memory_mib
    );
    let need = if ctx.args.vms > 0 { ctx.args.vms } else { ctx.args.min_vms };
    if max < need {
        return Ok(Err(format!("requires memory for {need} VMs: this machine holds {max} ({detail})")));
    }
    let max = match (ctx.args.vms, ctx.args.max_vms) {
        (0, 0) => max,
        (0, cap) => max.min(cap.max(need)),
        (vms, _) => vms,
    };
    Ok(Ok(KindPlan { min: need, max, detail, image: None }))
}

/// Free pod slots on the node: allocatable pods less the pods already bound
/// there (cluster read of `pods`; unreadable, the whole allocatable counts).
async fn container_preflight(ctx: &Ctx, node: &Value) -> Result<Result<KindPlan, String>, Infra> {
    let infra = |e: anyhow::Error| Infra(format!("{e:#}"));
    let image = match &ctx.args.image {
        Some(i) => i.clone(),
        None => {
            let r = ctx.kube.get(&format!("/api/v1/namespaces/{}/pods", ctx.namespace)).await.map_err(infra)?;
            let host = std::env::var("HOSTNAME").unwrap_or_default();
            match own_image(&kube::items(&r.body), &host) {
                Some(i) => i,
                None => return Err(Infra(format!("cannot learn this test's image from its pod in {} ({}); pass --image", ctx.namespace, r.code))),
            }
        }
    };
    let alloc = node["status"]["allocatable"]["pods"]
        .as_str()
        .and_then(|p| p.parse::<usize>().ok())
        .or_else(|| node["status"]["allocatable"]["pods"].as_u64().map(|p| p as usize))
        .ok_or_else(|| Infra(format!("node {:?} reports no allocatable pods", ctx.node_name)))?;
    let name = ctx.node_name.clone().unwrap_or_default();
    let used = match ctx.kube.get(&format!("/api/v1/pods?fieldSelector=spec.nodeName%3D{name}")).await {
        Ok(r) if r.ok() => Some(
            kube::items(&r.body)
                .iter()
                .filter(|p| !matches!(p["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed")))
                .count(),
        ),
        _ => None,
    };
    let free = alloc.saturating_sub(used.unwrap_or(0));
    let max = (free as f64 * ctx.args.pod_fraction) as usize;
    let detail = format!(
        "containers: {} free pod slots of {alloc} ({}) × {}; claims {} MiB, class {}",
        free,
        used.map_or("pods on the node unreadable".to_string(), |u| format!("{u} in use")),
        ctx.args.pod_fraction,
        ctx.args.claim_size_mib,
        ctx.args.storage_class.as_deref().unwrap_or("default"),
    );
    let need = if ctx.args.pods > 0 { ctx.args.pods } else { ctx.args.min_pods };
    if max < need {
        return Ok(Err(format!("requires {need} free pod slots: this node has {max} ({detail})")));
    }
    let max = match (ctx.args.pods, ctx.args.max_pods) {
        (0, 0) => max,
        (0, cap) => max.min(cap.max(need)),
        (pods, _) => pods,
    };
    Ok(Ok(KindPlan { min: need, max, detail, image: Some(image) }))
}

/// `/test container-waves` (#17): the container waves sized for a day run
/// (`[container-waves] budget_secs = 900`; by day a test fits 15 min, the
/// 8 h `long` runs only at night on a pve VM, stormcentral#325). Three waves
/// of 10, 20 and 15 pods: ramp, a varying size, repeat; a step waits at
/// most 240 s, so it reports inside the window. Put before the
/// caller's flags, so theirs win.
pub const CONTAINER_WAVES: [&str; 8] = ["--kinds", "containers", "--waves", "3", "--max-pods", "20", "--ready-timeout", "240"];

/// `/test vm-waves` (#16): the VM waves sized for a day run
/// (`[vm-waves] budget_secs = 1800`, the most a day run may take). Two
/// waves, 2 VMs then up to 10 (as many as the node holds; C2NR0Q2's free
/// memory holds 3, so a floor of 5 only ever skipped): ssh and RDP up,
/// a package installed and kept across a restart, drained. A VM waits at
/// most 600 s to come up and 300 s for its install, so a stuck step reports
/// inside the window.
pub const VM_WAVES: [&str; 14] = [
    "--kinds", "vms", "--waves", "2", "--min-vms", "2", "--max-vms", "10", "--ready-timeout", "600", "--install-timeout", "300",
    "--drain-timeout", "240",
];

/// argv for long's parser: a day suite's preset (`container-waves`,
/// `vm-waves`) goes after the program name, before the caller's flags.
pub fn argv(mut argv: Vec<String>, mode: &str) -> Vec<String> {
    let preset: &[&str] = match mode {
        "container-waves" => &CONTAINER_WAVES,
        "vm-waves" => &VM_WAVES,
        _ => &[],
    };
    let at = argv.len().min(1);
    argv.splice(at..at, preset.iter().map(|s| s.to_string()));
    argv
}

/// Wave `k` (of its kind)'s size: the first is the smallest (the latency
/// baseline), then it varies across the range.
fn wave_size(k: usize, min: usize, max: usize) -> usize {
    const MIX: [f64; 7] = [0.0, 1.0, 0.5, 0.0, 0.75, 1.0, 0.25];
    min + ((max - min) as f64 * MIX[k % MIX.len()]).round() as usize
}

#[derive(Debug, Clone, Serialize)]
struct WaveRecord {
    wave: usize,
    kind: Kind,
    /// VMs or pods.
    size: usize,
    ms: u64,
    failed: usize,
    /// VMs: create → ssh login; pods: create → Ready.
    start_ms_median: Option<u64>,
    start_ms_max: Option<u64>,
    /// VMs: restart → ssh again. Pods: all Ready → all restarted in place.
    restart_ms: Option<u64>,
    /// Pods: wave start → every pod Ready.
    ready_all_ms: Option<u64>,
    /// Pods: deleted → every replacement Ready with its claim.
    reschedule_ms: Option<u64>,
    /// Delete → nothing left.
    drained_ms: u64,
    residue: census::Census,
    left_behind: census::Own,
    regressions: Vec<String>,
}

pub(crate) fn median(mut v: Vec<u64>) -> Option<u64> {
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
        ("pod veths", c.veths.map(|v| v as u64), 0),
        ("PersistentVolumes", c.pvs.map(|v| v as u64), 0),
        ("cgroups", c.cgroups, a.cgroup_slack),
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

/// Which plan runs wave `k` (0-based), and its index among that kind's waves.
fn schedule(k: usize, kinds: usize) -> (usize, usize) {
    (k % kinds, k / kinds)
}

async fn soak(ctx: Arc<Ctx>, started: Instant, plans: &[Plan]) {
    let reserve = Duration::from_secs(ctx.args.drain_timeout + 60);
    let window_end = started + Duration::from_secs(ctx.args.timeout).saturating_sub(reserve);
    let baseline = ctx.sources().take().await;
    ctx.out.emit(Line {
        wave: Some(json!(baseline)),
        ..Line::new("baseline", Status::Pass, started.elapsed(), "node census before the first wave")
    });

    // A leftover source the waves' drain relies on and cannot read would
    // make "nothing left" pass unchecked: that part could not run.
    for (source, why) in unmeasured(&baseline, plans, ctx.stormblock_token) {
        ctx.could_not_run(format!("residue/{source}"), started.elapsed(), why);
    }

    let mut records: Vec<WaveRecord> = Vec::new();
    for k in 0.. {
        if ctx.args.waves > 0 && k >= ctx.args.waves {
            break;
        }
        let (pi, j) = schedule(k, plans.len());
        let plan = &plans[pi];
        let size = wave_size(j, plan.min, plan.max);
        if let Some(last) = records.iter().rev().find(|r| r.kind == plan.kind) {
            let estimate = Duration::from_millis(last.ms).mul_f64((size as f64 / last.size.max(1) as f64).max(1.0) * 1.2);
            if Instant::now() + estimate > window_end {
                break;
            }
        } else if k > 0 && Instant::now() >= window_end {
            break;
        }
        let rec = run_wave(&ctx, k + 1, plan, size, window_end, &baseline, &records).await;
        records.push(rec);
        let _ = tokio::fs::write(ctx.results.join("waves.json"), serde_json::to_vec_pretty(&records).unwrap_or_default()).await;
    }
    if records.is_empty() {
        ctx.out.emit(Line::new("long", Status::Fail, started.elapsed(), "the window closed before a single wave"));
    }
}

/// The leftover sources the planned waves' drain needs but the baseline
/// census could not read: `(source, why)`.
fn unmeasured(c: &census::Census, plans: &[Plan], token: bool) -> Vec<(&'static str, String)> {
    let vms = plans.iter().any(|p| p.kind == Kind::Vms);
    let pods = plans.iter().any(|p| p.kind == Kind::Containers);
    let mut out = Vec::new();
    if c.volumes.is_none() {
        out.push((
            "stormblock",
            format!(
                "stormblock's volume API did not answer ({}), so leftover volumes cannot be seen",
                if token { "token sent" } else { "no token found: STORMBLOCK_API_TOKEN, STORMBLOCK_TOKEN_FILE, /etc/stormblock/api_token or <host>/run/stormblock/engine/api_token" }
            ),
        ));
    }
    if vms && c.registrations.is_none() {
        out.push(("stormvm", "stormvm's API (loopback-only on the node) did not answer: the Job needs hostNetwork".into()));
    }
    if (vms || pods) && c.taps.is_none() {
        out.push(("host-network", "/proc/net/dev is not the host's, so taps and veths cannot be counted: the Job needs hostNetwork".into()));
    }
    if pods && c.pvs.is_none() {
        out.push(("persistentvolumes", "PersistentVolumes cannot be listed (cluster read of persistentvolumes)".into()));
    }
    out
}

/// The outcome of a wave's ramp + hold, before the drain.
#[derive(Default)]
struct Held {
    failed: usize,
    start_ms: Vec<u64>,
    restart_ms: Option<u64>,
    ready_all_ms: Option<u64>,
    reschedule_ms: Option<u64>,
}

async fn run_wave(
    ctx: &Arc<Ctx>,
    wave: usize,
    plan: &Plan,
    size: usize,
    window_end: Instant,
    baseline: &census::Census,
    prior: &[WaveRecord],
) -> WaveRecord {
    let t0 = Instant::now();
    let (held, left, d0) = match plan.kind {
        Kind::Vms => {
            let names: Vec<String> = ctx.names[..size].to_vec();
            let held = vm_hold(ctx, wave, &names, window_end).await;
            let d0 = Instant::now();
            (held, wave::drain(ctx, &names).await.unwrap_or_default(), d0)
        }
        Kind::Containers => {
            let apps = containers::app_names(ctx, wave, size);
            let image = plan.image.clone().unwrap_or_default();
            let hold = containers::hold(ctx, wave, &apps, &image);
            let remaining = window_end.saturating_duration_since(Instant::now()).max(Duration::from_secs(60));
            let held = match tokio::time::timeout(remaining, hold).await {
                Ok(o) => Held {
                    failed: o.failed,
                    start_ms: o.ready_ms,
                    restart_ms: o.restart_ms,
                    ready_all_ms: o.ready_all_ms,
                    reschedule_ms: o.reschedule_ms,
                },
                Err(_) => {
                    ctx.out.emit(Line::new(format!("wave-{wave}/hold"), Status::Fail, t0.elapsed(), "the window closed mid-wave"));
                    Held { failed: size, ..Default::default() }
                }
            };
            let d0 = Instant::now();
            (held, containers::drain(ctx, wave, &apps).await, d0)
        }
    };
    let drained_ms = d0.elapsed().as_millis() as u64;
    let what = match plan.kind {
        Kind::Vms => "VMs",
        Kind::Containers => "Deployments, their claims and the Service",
    };
    if left.is_empty() {
        ctx.out.emit(Line::new(format!("wave-{wave}/drain"), Status::Pass, d0.elapsed(), format!("{size} {what} deleted, nothing of the run left")));
    } else {
        ctx.out.emit(Line::new(format!("wave-{wave}/drain"), Status::Fail, d0.elapsed(), format!("left behind after {}s: {}", ctx.args.drain_timeout, json!(left))));
    }
    let residue = ctx.sources().take().await;

    let start_ms_median = median(held.start_ms.clone());
    let mut regress = regressions(baseline, prior.last().map(|p| &p.residue), &residue, &ctx.args);
    let first = prior.iter().find(|r| r.kind == plan.kind).and_then(|f| f.start_ms_median);
    if let (Some(first), Some(now)) = (first, start_ms_median) {
        let limit = (first as f64 * ctx.args.slowdown) as u64 + ctx.args.slowdown_grace_secs * 1000;
        if now > limit {
            regress.push(format!("median start {now} ms vs {first} ms in the first {} wave (limit {limit} ms)", plan.kind.name()));
        }
    }
    let rec = WaveRecord {
        wave,
        kind: plan.kind,
        size,
        ms: t0.elapsed().as_millis() as u64,
        failed: held.failed,
        start_ms_median,
        start_ms_max: held.start_ms.iter().copied().max(),
        restart_ms: held.restart_ms,
        ready_all_ms: held.ready_all_ms,
        reschedule_ms: held.reschedule_ms,
        drained_ms,
        residue,
        left_behind: left,
        regressions: regress,
    };
    let ok = rec.failed == 0 && rec.left_behind.is_empty() && rec.regressions.is_empty();
    let unit = match plan.kind {
        Kind::Vms => "VMs",
        Kind::Containers => "pods",
    };
    let mut detail = format!("{} wave: {size} {unit}, {} failed, drained in {drained_ms} ms", plan.kind.name(), rec.failed);
    if !rec.regressions.is_empty() {
        detail.push_str(&format!("; regressed: {}", rec.regressions.join("; ")));
    }
    ctx.out.emit(Line {
        wave: Some(serde_json::to_value(&rec).unwrap_or(Value::Null)),
        ..Line::new(format!("wave-{wave}"), if ok { Status::Pass } else { Status::Fail }, t0.elapsed(), detail)
    });
    rec
}

/// Ramp + hold a VM wave, every VM concurrently.
async fn vm_hold(ctx: &Arc<Ctx>, wave: usize, names: &[String], window_end: Instant) -> Held {
    let t0 = Instant::now();
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
    Held {
        failed: vms.iter().filter(|v| v.failed).count() + names.len().saturating_sub(vms.len()),
        start_ms: vms.iter().filter_map(|v| v.start_ms).collect(),
        restart_ms: median(vms.iter().filter_map(|v| v.restart_ms).collect()),
        ..Default::default()
    }
}

/// Leave the host as found: the run's VMs, key Secret, and container waves'
/// Deployments, Services and claims. The namespace itself is the runner's
/// to delete.
async fn cleanup(ctx: &Ctx) {
    let ns = &ctx.namespace;
    let run = format!("?labelSelector=storm.io/test-run%3D{}", ctx.run_id);
    if let Ok(r) = ctx.kube.get(&format!("{}{run}", kube::vms(ns))).await {
        for vm in kube::items(&r.body) {
            if let Some(n) = vm["metadata"]["name"].as_str() {
                let _ = ctx.kube.delete(&format!("{}/{n}", kube::vms(ns))).await;
            }
        }
    }
    let _ = ctx.kube.delete(&format!("/api/v1/namespaces/{ns}/secrets/{}", ctx.secret_name())).await;
    let sel = format!("?labelSelector=app.kubernetes.io/managed-by%3D{}", containers::MANAGED_BY);
    for base in [
        format!("/apis/apps/v1/namespaces/{ns}/deployments"),
        format!("/api/v1/namespaces/{ns}/services"),
        format!("/api/v1/namespaces/{ns}/persistentvolumeclaims"),
    ] {
        if let Ok(r) = ctx.kube.get(&format!("{base}{sel}")).await {
            for o in kube::items(&r.body) {
                if let Some(n) = o["metadata"]["name"].as_str() {
                    let _ = ctx.kube.delete(&format!("{base}/{n}")).await;
                }
            }
        }
    }
}

/// This test's image, from its own pod in the run namespace: the pod named
/// `$HOSTNAME`, or — under `hostNetwork`, where `$HOSTNAME` is the node's —
/// the one running `/test` that no wave made (the Job's).
pub(crate) fn own_image(pods: &[Value], hostname: &str) -> Option<String> {
    let image = |p: &Value| p["spec"]["containers"][0]["image"].as_str().map(str::to_string);
    if let Some(p) = pods.iter().find(|p| p["metadata"]["name"].as_str() == Some(hostname)) {
        return image(p);
    }
    let mut own: Vec<String> = pods
        .iter()
        .filter(|p| p["metadata"]["labels"]["app.kubernetes.io/managed-by"].is_null())
        .filter(|p| p["spec"]["containers"][0]["command"][0].as_str() == Some("/test"))
        .filter(|p| !matches!(p["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed")))
        .filter_map(image)
        .collect();
    own.dedup();
    if own.len() == 1 { own.pop() } else { None }
}

/// A Node's `kubernetes.io/hostname` label, what a nodeSelector pins on.
fn hostname_label(node: &Value) -> Option<String> {
    node["metadata"]["labels"]["kubernetes.io/hostname"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_waves_preset() {
        let a = Args::parse_from(argv(vec!["/test".into()], "container-waves"));
        assert_eq!((a.kinds.clone(), a.waves, a.max_pods, a.pods), (vec![Kind::Containers], 3, 20, 0));
        // A flag after the preset wins.
        let a = Args::parse_from(argv(vec!["/test".into(), "--waves".into(), "1".into(), "--kinds".into(), "containers,vms".into()], "container-waves"));
        assert_eq!((a.kinds, a.waves), (vec![Kind::Containers, Kind::Vms], 1));
        let a = Args::parse_from(argv(vec!["/test".into()], "long"));
        assert_eq!((a.kinds, a.waves, a.max_pods), (vec![Kind::Containers, Kind::Vms], 0, 0));
        let a = Args::parse_from(argv(vec!["/test".into()], "vm-waves"));
        assert_eq!((a.kinds, a.waves, a.min_vms, a.max_vms, a.ready_timeout), (vec![Kind::Vms], 2, 2, 10, 600));
    }

    #[test]
    fn pinned_by_the_hostname_label() {
        let n = serde_json::json!({ "metadata": { "name": "n1", "labels": { "kubernetes.io/hostname": "h1" } } });
        assert_eq!(hostname_label(&n).as_deref(), Some("h1"));
        assert_eq!(hostname_label(&serde_json::json!({ "metadata": { "name": "n1" } })), None);
    }

    #[test]
    fn waves_start_smallest_and_stay_in_range() {
        assert_eq!(wave_size(0, 10, 40), 10);
        assert_eq!(wave_size(1, 10, 40), 40);
        for k in 0..30 {
            let s = wave_size(k, 10, 40);
            assert!((10..=40).contains(&s));
        }
        assert_eq!(wave_size(5, 10, 10), 10);
        // container-waves' three: 10, 20, 15.
        assert_eq!((0..3).map(|k| wave_size(k, 10, 20)).collect::<Vec<_>>(), vec![10, 20, 15]);
    }

    #[test]
    fn kinds_take_turns_and_each_starts_smallest() {
        assert_eq!(schedule(0, 2), (0, 0));
        assert_eq!(schedule(1, 2), (1, 0));
        assert_eq!(schedule(2, 2), (0, 1));
        assert_eq!(schedule(3, 1), (0, 3));
        let a = Args::parse_from(["test"]);
        assert_eq!(a.kinds, vec![Kind::Containers, Kind::Vms]);
        let a = Args::parse_from(["test", "--kinds", "vms"]);
        assert_eq!(a.kinds, vec![Kind::Vms]);
    }

    #[test]
    fn the_image_is_the_jobs() {
        let pod = |name: &str, image: &str, managed: bool| {
            let mut p = json!({"metadata": {"name": name, "labels": {}}, "spec": {"containers": [{"image": image, "command": ["/test"]}]}, "status": {"phase": "Running"}});
            if managed {
                p["metadata"]["labels"]["app.kubernetes.io/managed-by"] = json!("stormcos_qa-containers");
            }
            p
        };
        let pods = vec![pod("job-abc", "test-x:1", false), pod("ct-w1-001-z", "test-x:1", true)];
        assert_eq!(own_image(&pods, "job-abc").as_deref(), Some("test-x:1"));
        // hostNetwork: $HOSTNAME is the node's.
        assert_eq!(own_image(&pods, "storm-06f96d").as_deref(), Some("test-x:1"));
        let two = vec![pod("a", "test-x:1", false), pod("b", "test-y:2", false)];
        assert_eq!(own_image(&two, "node"), None);
    }

    #[test]
    fn an_unreadable_leftover_source_is_not_a_pass() {
        let plan = |kind| Plan { kind, min: 1, max: 1, image: None };
        let all = census::Census { volumes: Some(0), registrations: Some(0), taps: Some(0), pvs: Some(0), ..Default::default() };
        assert!(unmeasured(&all, &[plan(Kind::Vms), plan(Kind::Containers)], true).is_empty());
        let none = census::Census::default();
        let v: Vec<_> = unmeasured(&none, &[plan(Kind::Vms)], false).into_iter().map(|(s, _)| s).collect();
        assert_eq!(v, ["stormblock", "stormvm", "host-network"]);
        let c: Vec<_> = unmeasured(&none, &[plan(Kind::Containers)], false).into_iter().map(|(s, _)| s).collect();
        assert_eq!(c, ["stormblock", "host-network", "persistentvolumes"]);
    }

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3, 1, 2]), Some(2));
    }

    #[test]
    fn growth_is_a_regression_a_plateau_is_not() {
        let a = Args::parse_from(["test"]);
        let c = |v| census::Census { volumes: Some(v), ..Default::default() };
        assert!(regressions(&c(5), None, &c(5), &a).is_empty());
        assert_eq!(regressions(&c(5), None, &c(6), &a).len(), 1);
        assert_eq!(regressions(&c(5), Some(&c(6)), &c(7), &a).len(), 1);
        assert!(regressions(&c(5), Some(&c(7)), &c(7), &a).is_empty());
        // Unmeasured is never a regression, and never a silent zero.
        assert!(regressions(&census::Census::default(), None, &c(9), &a).is_empty());
        // cgroups have slack.
        let g = |v| census::Census { cgroups: Some(v), ..Default::default() };
        assert!(regressions(&g(100), None, &g(110), &a).is_empty());
        assert_eq!(regressions(&g(100), None, &g(117), &a).len(), 1);
    }
}
