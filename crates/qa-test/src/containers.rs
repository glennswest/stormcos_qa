//! One **container** wave of `/test long` (#17): ramp N Deployments, hold
//! (Ready behind a Service → restart in place → reschedule, each proving the
//! claim kept its data), drain (all of it gone, on the API and on the node).
//!
//! Every pod has its own claim, and a stormblock claim is ReadWriteOnce, so
//! a wave of N pods is N Deployments of one replica each, with one claim per
//! Deployment. The claims use the default StorageClass, which on stormcos is
//! the built-in `stormblock` driver (`--storage-class` overrides it). One
//! Service selects the whole wave. The pods run this image as `/test claim`
//! (`claim.rs`), which writes its claim or checks it and logs which, so every
//! data check reads the pod log through the API and needs no pod network.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::SERVE_PORT;
use crate::census::Own;
use crate::kube;
use crate::long::Ctx;
use crate::report::{Line, Status};

/// `app.kubernetes.io/managed-by` on everything a container wave makes: how
/// the census tells it from the Job's own pod.
pub const MANAGED_BY: &str = "stormcos_qa-containers";

/// Evidence files written per step, at most.
const EVIDENCE: usize = 5;

#[derive(Debug, Default)]
pub struct WaveOut {
    /// create → first Ready, per Deployment that got there.
    pub ready_ms: Vec<u64>,
    /// Wave start → every Deployment Ready.
    pub ready_all_ms: Option<u64>,
    /// All Ready → every pod restarted in place and Ready, claim intact.
    pub restart_ms: Option<u64>,
    /// Pods deleted → every replacement Ready, claim intact.
    pub reschedule_ms: Option<u64>,
    /// Deployments that failed any step.
    pub failed: usize,
}

pub fn app_names(ctx: &Ctx, wave: usize, size: usize) -> Vec<String> {
    (1..=size).map(|i| format!("{}-w{wave}-{i:03}", ctx.prefix_containers())).collect()
}

fn labels(ctx: &Ctx, wave: usize) -> Value {
    json!({
        "storm.io/test-run": ctx.run_id,
        "storm.io/test-wave": wave.to_string(),
        "app.kubernetes.io/managed-by": MANAGED_BY,
    })
}

fn app_labels(ctx: &Ctx, wave: usize, app: &str) -> Value {
    let mut l = labels(ctx, wave);
    l["storm.io/qa-app"] = json!(app);
    l
}

/// The token a Deployment's claim must hold.
fn token(ctx: &Ctx, app: &str) -> String {
    format!("{}/{app}", ctx.run_id)
}

pub fn claim_manifest(ctx: &Ctx, wave: usize, app: &str) -> Value {
    let mut spec = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": {"requests": {"storage": format!("{}Mi", ctx.args.claim_size_mib)}},
    });
    if let Some(c) = &ctx.args.storage_class {
        spec["storageClassName"] = json!(c);
    }
    json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": app, "labels": app_labels(ctx, wave, app)},
        "spec": spec,
    })
}

pub fn deployment_manifest(ctx: &Ctx, wave: usize, app: &str, image: &str) -> Value {
    let mut pod = json!({
        "terminationGracePeriodSeconds": 1,
        "automountServiceAccountToken": false,
        "containers": [{
            "name": "c",
            "image": image,
            "command": ["/test"],
            "args": ["claim"],
            "env": [
                {"name": "CLAIM_TOKEN", "value": token(ctx, app)},
                {"name": "CLAIM_EXIT_ONCE_AFTER", "value": ctx.args.restart_after.to_string()},
            ],
            "ports": [{"containerPort": SERVE_PORT, "protocol": "TCP"}],
            "readinessProbe": {"tcpSocket": {"port": SERVE_PORT}, "periodSeconds": 2, "failureThreshold": 1},
            "resources": {"requests": {"memory": format!("{}Mi", ctx.args.pod_memory_mib), "cpu": "5m"}},
            "volumeMounts": [{"name": "data", "mountPath": "/data"}],
        }],
        "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": app}}],
    });
    if let Some(h) = &ctx.node_hostname {
        pod["nodeSelector"] = json!({"kubernetes.io/hostname": h});
    }
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": app, "labels": app_labels(ctx, wave, app)},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"storm.io/qa-app": app}},
            // A claim is ReadWriteOnce: the old pod lets go before the new one.
            "strategy": {"type": "Recreate"},
            "template": {"metadata": {"labels": app_labels(ctx, wave, app)}, "spec": pod},
        }
    })
}

pub fn service_manifest(ctx: &Ctx, wave: usize, name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": name, "labels": labels(ctx, wave)},
        "spec": {
            "selector": {"storm.io/test-run": ctx.run_id, "storm.io/test-wave": wave.to_string()},
            "ports": [{"port": SERVE_PORT, "targetPort": SERVE_PORT, "protocol": "TCP"}],
        }
    })
}

fn deployments(ns: &str) -> String {
    format!("/apis/apps/v1/namespaces/{ns}/deployments")
}
fn claims(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/persistentvolumeclaims")
}
fn services(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/services")
}
fn pods(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/pods")
}

/// A pod as the wave sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PodView {
    pub name: String,
    pub app: String,
    pub ready: bool,
    pub restarts: u64,
    pub terminating: bool,
    /// The container's state, for a failure's reason: `running`,
    /// `waiting:<reason>` or `terminated:<reason>:<exit code>`.
    pub state: String,
}

pub fn pod_views(list: &Value) -> Vec<PodView> {
    kube::items(list)
        .iter()
        .filter_map(|p| {
            let s = &p["status"];
            Some(PodView {
                name: p["metadata"]["name"].as_str()?.to_string(),
                app: p["metadata"]["labels"]["storm.io/qa-app"].as_str()?.to_string(),
                ready: s["conditions"].as_array().into_iter().flatten().any(|c| c["type"] == "Ready" && c["status"] == "True"),
                restarts: s["containerStatuses"].as_array().into_iter().flatten().filter_map(|c| c["restartCount"].as_u64()).sum(),
                terminating: !p["metadata"]["deletionTimestamp"].is_null(),
                state: container_state(&s["containerStatuses"][0]["state"]),
            })
        })
        .collect()
}

fn container_state(st: &Value) -> String {
    if st["running"].is_object() {
        "running".into()
    } else if let Some(w) = st["waiting"].as_object() {
        format!("waiting:{}", w.get("reason").and_then(Value::as_str).unwrap_or("?"))
    } else if let Some(t) = st["terminated"].as_object() {
        format!(
            "terminated:{}:{}",
            t.get("reason").and_then(Value::as_str).unwrap_or("?"),
            t.get("exitCode").and_then(Value::as_i64).map_or("?".to_string(), |c| c.to_string())
        )
    } else {
        "unknown".into()
    }
}

/// What a pod's log says about its claim: `Ok(true)` found intact,
/// `Ok(false)` not said yet, `Err` a mismatch or an error.
pub fn claim_state(log: &str) -> Result<bool, String> {
    let mut found = false;
    for l in log.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        match v["claim"].as_str() {
            Some("found") => found = true,
            Some("mismatch") | Some("error") => return Err(format!("{}: {}", v["claim"], v["detail"])),
            _ => {}
        }
    }
    Ok(found)
}

async fn list_pods(ctx: &Ctx, wave: usize) -> Vec<PodView> {
    let path = format!(
        "{}?labelSelector=storm.io/test-run%3D{},storm.io/test-wave%3D{wave}",
        pods(&ctx.namespace),
        ctx.run_id
    );
    match ctx.kube.get(&path).await {
        Ok(r) if r.ok() => pod_views(&r.body),
        _ => Vec::new(),
    }
}

async fn pod_log(ctx: &Ctx, pod: &str) -> String {
    match ctx.kube.get(&format!("{}/{pod}/log", pods(&ctx.namespace))).await {
        Ok(r) if r.ok() => match r.body {
            Value::String(s) => s,
            v => v.to_string(),
        },
        Ok(r) => format!("(log answered {})", r.code),
        Err(e) => format!("(log: {e:#})"),
    }
}

/// A failing Deployment's pods, claim and events, under /results.
async fn evidence(ctx: &Ctx, wave: usize, app: &str) -> String {
    let ns = &ctx.namespace;
    let mut ev = json!({});
    let sel = format!("?labelSelector=storm.io/qa-app%3D{app}");
    for (k, p) in [
        ("deployment", format!("{}/{app}", deployments(ns))),
        ("pods", format!("{}{sel}", pods(ns))),
        ("claim", format!("{}/{app}", claims(ns))),
        ("events", format!("/api/v1/namespaces/{ns}/events")),
    ] {
        ev[k] = match ctx.kube.get(&p).await {
            Ok(r) if k == "events" => json!(kube::items(&r.body)
                .into_iter()
                .filter(|e| e["involvedObject"]["name"].as_str().is_some_and(|n| n.starts_with(app)))
                .collect::<Vec<_>>()),
            Ok(r) => r.body,
            Err(e) => json!(e.to_string()),
        };
    }
    let file = ctx.results.join(format!("wave-{wave}-{app}.json"));
    let _ = tokio::fs::write(&file, serde_json::to_vec_pretty(&ev).unwrap_or_default()).await;
    file.display().to_string()
}

/// Emit one line for a step over the whole wave; the failed Deployments get
/// evidence files (the first few).
async fn step_line(ctx: &Ctx, wave: usize, step: &str, took: Duration, ok_detail: String, failed: &BTreeMap<String, String>) {
    if failed.is_empty() {
        ctx.out.emit(Line::new(format!("wave-{wave}/{step}"), Status::Pass, took, ok_detail));
        return;
    }
    let mut files = Vec::new();
    for app in failed.keys().take(EVIDENCE) {
        files.push(evidence(ctx, wave, app).await);
    }
    let list: Vec<String> = failed.iter().take(20).map(|(a, why)| format!("{a}: {why}")).collect();
    ctx.out.emit(Line::new(
        format!("wave-{wave}/{step}"),
        Status::Fail,
        took,
        format!("{} of the wave failed: {}{}; evidence in {}", failed.len(), list.join("; "), if failed.len() > 20 { "; …" } else { "" }, files.join(", ")),
    ));
}

/// Ramp and hold one container wave. The caller drains it with [`drain`].
pub async fn hold(ctx: &Arc<Ctx>, wave: usize, apps: &[String], image: &str) -> WaveOut {
    let mut out = WaveOut::default();
    let ns = ctx.namespace.clone();
    let t0 = Instant::now();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();

    // Ramp: the Service, then a claim and a Deployment per pod.
    let svc = format!("{}-w{wave}", ctx.prefix_containers());
    if let Err(why) = create(ctx, &services(&ns), &service_manifest(ctx, wave, &svc)).await {
        ctx.out.emit(Line::new(format!("wave-{wave}/service"), Status::Fail, t0.elapsed(), why));
    }
    let mut created: BTreeMap<String, Instant> = BTreeMap::new();
    for app in apps {
        let c = Instant::now();
        let r = match create(ctx, &claims(&ns), &claim_manifest(ctx, wave, app)).await {
            Ok(()) => create(ctx, &deployments(&ns), &deployment_manifest(ctx, wave, app, image)).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => {
                created.insert(app.clone(), c);
            }
            Err(why) => {
                failed.insert(app.clone(), why);
            }
        }
    }

    // Ready: every Deployment's pod, once.
    let deadline = Instant::now() + Duration::from_secs(ctx.args.ready_timeout);
    let mut ready: BTreeMap<String, u64> = BTreeMap::new();
    loop {
        for p in list_pods(ctx, wave).await {
            if (p.ready || p.restarts > 0) && !ready.contains_key(&p.app)
                && let Some(c) = created.get(&p.app)
            {
                // A pod already restarted was Ready in between (its readiness
                // is what the restart below needs anyway): count it now.
                ready.insert(p.app.clone(), c.elapsed().as_millis() as u64);
            }
        }
        if ready.len() == created.len() || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for app in created.keys().filter(|a| !ready.contains_key(*a)) {
        failed.insert(app.clone(), format!("not Ready in {}s", ctx.args.ready_timeout));
    }
    out.ready_ms = ready.values().copied().collect();
    if ready.len() == apps.len() {
        out.ready_all_ms = Some(t0.elapsed().as_millis() as u64);
    }
    let median = crate::long::median(out.ready_ms.clone());
    step_line(
        ctx,
        wave,
        "ready",
        t0.elapsed(),
        format!("{} Deployments Ready (median create→Ready {} ms)", apps.len(), median.unwrap_or(0)),
        &failed,
    )
    .await;
    let alive: Vec<String> = ready.keys().cloned().collect();

    // Service: endpoints for every Ready pod, and the ClusterIP answers.
    let s0 = Instant::now();
    let svc_check = service(ctx, &svc, alive.len()).await;
    match svc_check {
        Ok(d) => ctx.out.emit(Line::new(format!("wave-{wave}/service"), Status::Pass, s0.elapsed(), d)),
        Err(e) => ctx.out.emit(Line::new(format!("wave-{wave}/service"), Status::Fail, s0.elapsed(), e)),
    }

    // Restart in place: `/test claim` exits once; the kubelet restarts it on
    // the same pod, and the claim must still hold what it wrote.
    let r0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(ctx.args.restart_after + ctx.args.ready_timeout);
    let mut step_failed = BTreeMap::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    // What each pod last looked like, for the reason of one that never made it.
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    while done.len() < alive.len() && Instant::now() < deadline {
        for p in list_pods(ctx, wave).await {
            if alive.contains(&p.app) && !done.contains(&p.app) {
                last.insert(p.app.clone(), format!("last seen {}: ready {}, restarts {}, {}", p.name, p.ready, p.restarts, p.state));
            }
            if !alive.contains(&p.app) || done.contains(&p.app) || p.terminating || !(p.ready && p.restarts > 0) {
                continue;
            }
            match claim_state(&pod_log(ctx, &p.name).await) {
                Ok(true) => {
                    done.insert(p.app.clone());
                }
                Ok(false) => {}
                Err(e) => {
                    step_failed.insert(p.app.clone(), format!("{}: after the restart the claim {e}", p.name));
                    done.insert(p.app.clone());
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for app in alive.iter().filter(|a| !done.contains(*a)) {
        let seen = last.get(app).map_or("never listed".to_string(), String::clone);
        step_failed.insert(app.clone(), format!("not restarted and Ready with its claim found in time ({seen})"));
    }
    if step_failed.is_empty() {
        out.restart_ms = Some(r0.elapsed().as_millis() as u64);
    }
    step_line(ctx, wave, "restart", r0.elapsed(), format!("{} pods restarted in place, claims intact", alive.len()), &step_failed).await;
    failed.extend(step_failed);

    // Reschedule: delete every pod; each Deployment's replacement must come
    // up and read back the claim the old pod wrote.
    let q0 = Instant::now();
    let mut old: BTreeMap<String, String> = BTreeMap::new();
    for p in list_pods(ctx, wave).await {
        if alive.contains(&p.app) && !failed.contains_key(&p.app) && !p.terminating {
            old.insert(p.app.clone(), p.name.clone());
        }
    }
    for pod in old.values() {
        let _ = ctx.kube.delete(&format!("{}/{pod}", pods(&ns))).await;
    }
    let deadline = Instant::now() + Duration::from_secs(ctx.args.ready_timeout);
    let mut step_failed = BTreeMap::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    while done.len() < old.len() && Instant::now() < deadline {
        for p in list_pods(ctx, wave).await {
            let Some(prev) = old.get(&p.app) else { continue };
            if done.contains(&p.app) || p.name == *prev || p.terminating || !p.ready {
                continue;
            }
            match claim_state(&pod_log(ctx, &p.name).await) {
                Ok(true) => {
                    done.insert(p.app.clone());
                }
                Ok(false) => {}
                Err(e) => {
                    step_failed.insert(p.app.clone(), format!("{} replaced {prev}, and the claim {e}", p.name));
                    done.insert(p.app.clone());
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for app in old.keys().filter(|a| !done.contains(*a)) {
        step_failed.insert(app.clone(), format!("no replacement for {} Ready with its claim found in time", old[app]));
    }
    if step_failed.is_empty() && !old.is_empty() {
        out.reschedule_ms = Some(q0.elapsed().as_millis() as u64);
    }
    if old.is_empty() {
        // Nothing reached this step: it checked nothing, so it is no pass.
        ctx.out.emit(Line::new(format!("wave-{wave}/reschedule"), Status::Skip, q0.elapsed(), "no pod left to reschedule: every pod failed an earlier step"));
    } else {
        step_line(ctx, wave, "reschedule", q0.elapsed(), format!("{} pods replaced, each read back its claim", old.len()), &step_failed).await;
    }
    failed.extend(step_failed);

    out.failed = failed.len();
    out
}

async fn create(ctx: &Ctx, path: &str, body: &Value) -> Result<(), String> {
    match ctx.kube.post(path, body).await {
        Ok(r) if r.ok() => Ok(()),
        Ok(r) => Err(format!("creating {} answered {}: {}", body["metadata"]["name"], r.code, r.body)),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The wave's Service fronts every Ready pod: its Endpoints list them all,
/// and a connection to its ClusterIP is answered by one of them.
async fn service(ctx: &Ctx, svc: &str, want: usize) -> Result<String, String> {
    let ns = &ctx.namespace;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = String::from("no Endpoints object");
    let n = loop {
        let mut n = None;
        if let Ok(r) = ctx.kube.get(&format!("/api/v1/namespaces/{ns}/endpoints/{svc}")).await
            && r.ok()
        {
            n = Some(endpoints_ready(&r.body));
        } else if let Ok(r) = ctx
            .kube
            .get(&format!("/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices?labelSelector=kubernetes.io/service-name%3D{svc}"))
            .await
            && r.ok()
            && !kube::items(&r.body).is_empty()
        {
            n = Some(slices_ready(&r.body));
        }
        if let Some(n) = n {
            if n >= want {
                break n;
            }
            last = format!("{n} ready addresses of {want}");
        }
        if Instant::now() >= deadline {
            return Err(format!("Endpoints of {svc}: {last} after 60s"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let ip = match ctx.kube.get(&format!("{}/{svc}", services(ns))).await {
        Ok(r) if r.ok() => r.body["spec"]["clusterIP"].as_str().unwrap_or_default().to_string(),
        Ok(r) => return Err(format!("reading Service {svc} answered {}", r.code)),
        Err(e) => return Err(format!("{e:#}")),
    };
    if ip.is_empty() || ip == "None" {
        return Err(format!("Service {svc} has no ClusterIP"));
    }
    let answered = async {
        use tokio::io::AsyncReadExt;
        let mut s = tokio::net::TcpStream::connect((ip.as_str(), SERVE_PORT)).await?;
        let mut b = vec![0u8; 128];
        let n = s.read(&mut b).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&b[..n]).trim().to_string())
    };
    match tokio::time::timeout(Duration::from_secs(10), answered).await {
        Ok(Ok(a)) if a.starts_with("stormcos_qa claim") => Ok(format!("{n} endpoints; {ip}:{SERVE_PORT} answered ({a})")),
        Ok(Ok(a)) => Err(format!("{n} endpoints, but {ip}:{SERVE_PORT} answered {a:?}")),
        Ok(Err(e)) => Err(format!("{n} endpoints, but {ip}:{SERVE_PORT}: {e}")),
        Err(_) => Err(format!("{n} endpoints, but {ip}:{SERVE_PORT} did not answer in 10s")),
    }
}

/// Ready addresses in an Endpoints object.
pub fn endpoints_ready(ep: &Value) -> usize {
    ep["subsets"].as_array().into_iter().flatten().map(|s| s["addresses"].as_array().map_or(0, Vec::len)).sum()
}

/// Ready endpoints in a list of EndpointSlices (`ready` absent means ready).
pub fn slices_ready(list: &Value) -> usize {
    kube::items(list)
        .iter()
        .flat_map(|s| s["endpoints"].as_array().cloned().unwrap_or_default())
        .filter(|e| e["conditions"]["ready"].as_bool() != Some(false))
        .count()
}

/// Delete the wave's Deployments, Service and claims; wait until nothing of
/// it is left — Deployments, ReplicaSets, pods, claims, Services, PVs and the
/// claims' stormblock volumes. Returns what is left.
pub async fn drain(ctx: &Ctx, wave: usize, apps: &[String]) -> Own {
    let ns = &ctx.namespace;
    let _ = ctx.kube.delete(&format!("{}/{}-w{wave}", services(ns), ctx.prefix_containers())).await;
    for app in apps {
        let _ = ctx.kube.delete(&format!("{}/{app}", deployments(ns))).await;
        let _ = ctx.kube.delete(&format!("{}/{app}", claims(ns))).await;
    }
    let deadline = Instant::now() + Duration::from_secs(ctx.args.drain_timeout);
    loop {
        let own = ctx.sources().take().await.own;
        if own.is_empty() || Instant::now() >= deadline {
            return own;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_states_name_their_reason() {
        assert_eq!(container_state(&json!({"running": {"startedAt": "t"}})), "running");
        assert_eq!(container_state(&json!({"waiting": {"reason": "CrashLoopBackOff"}})), "waiting:CrashLoopBackOff");
        assert_eq!(container_state(&json!({"terminated": {"reason": "Completed", "exitCode": 0}})), "terminated:Completed:0");
        assert_eq!(container_state(&Value::Null), "unknown");
        // The claim helper's unspaced line still parses (rustkube-node#136).
        let l = crate::report::unspaced(&json!({"claim": "found", "detail": "token and 1 MiB blob intact"}).to_string());
        assert!(!l.contains(' '));
        assert_eq!(claim_state(&l), Ok(true));
        assert_eq!(crate::report::unspaced("{\"detail\":\"a b\"}"), "{\"detail\":\"a\u{b7}b\"}");
    }

    #[test]
    fn pods_are_read_by_their_app() {
        let list = json!({"items": [
            {"metadata": {"name": "a-1", "labels": {"storm.io/qa-app": "a"}},
             "status": {"conditions": [{"type": "Ready", "status": "True"}], "containerStatuses": [{"restartCount": 1}]}},
            {"metadata": {"name": "b-1", "labels": {"storm.io/qa-app": "b"}, "deletionTimestamp": "x"},
             "status": {"conditions": [{"type": "Ready", "status": "False"}]}},
            {"metadata": {"name": "job", "labels": {}}, "status": {}}
        ]});
        let v = pod_views(&list);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].ready, v[0].restarts, v[0].terminating), (true, 1, false));
        assert_eq!((v[1].ready, v[1].terminating), (false, true));
    }

    #[test]
    fn endpoints_and_slices() {
        let ep = json!({"subsets": [{"addresses": [{}, {}], "notReadyAddresses": [{}]}, {"addresses": [{}]}]});
        assert_eq!(endpoints_ready(&ep), 3);
        let sl = json!({"items": [{"endpoints": [{"conditions": {"ready": true}}, {"conditions": {"ready": false}}, {}]}]});
        assert_eq!(slices_ready(&sl), 2);
    }

    #[test]
    fn claim_state_from_the_log() {
        assert_eq!(claim_state("{\"claim\":\"written\",\"detail\":\"x\"}\n"), Ok(false));
        assert_eq!(claim_state("{\"claim\":\"written\"}\n{\"claim\":\"exiting\"}\n{\"claim\":\"found\"}\n"), Ok(true));
        assert!(claim_state("noise\n{\"claim\":\"mismatch\",\"detail\":\"token\"}\n").is_err());
        assert_eq!(claim_state(""), Ok(false));
    }
}
