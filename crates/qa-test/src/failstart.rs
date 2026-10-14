//! A VM wave's failed-start case (#31): a VM whose start fails on the node,
//! then is fixed, under the run strategy it declares.
//!
//! The failure is the one stormvm's own test container uses
//! (`stormvm-verify` `unwired`): an SR-IOV NIC on a network nothing wires.
//! It converts, and the node refuses it at start (`stormvm_net::plan`), so
//! the kubelet's failed-start path runs for real (rustkube-node#76: Pending,
//! `reason: FailedStart`, `start failed (attempt N), retrying in Ss`, 10 s
//! doubling to 5 min). stormvm's container checks that a failed start is
//! retried and that `Once` ends `Failed`; what only a soak shows is checked
//! here, at the start of each VM wave, on the node the last wave left
//! drained (so it never competes with the wave's VMs for memory):
//!
//! - `failed-start/retried`: under `runStrategy: Always`, the attempt count
//!   rises and the delay grows, bounded; the VM says it is failing (not
//!   `Starting`), with a `Failure` condition;
//! - `failed-start/no-residue`: while it fails, nothing of the run is on the
//!   node (volumes, stormvm registrations, taps): every failed attempt gave
//!   back what it made;
//! - `failed-start/recovered`: the template fixed (the NIC taken out) and the
//!   VM restarted, it comes up and takes the run's key over ssh;
//! - `failed-start/once`: under `runStrategy: Once`, the VMI ends `Failed`,
//!   the VM reads `Failed`, and it stays so (same VMI, no new attempt);
//! - `failed-start/drain`: both deleted, nothing of the run left.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::census::Own;
use crate::kube;
use crate::long::Ctx;
use crate::report::{Line, Status};
use crate::wave;

/// The NIC nothing wires (stormvm-verify's `unwired`).
const NIC: &str = "sriov0";
/// How long the failing VM gets to show its third attempt (10 s + 20 s of
/// backoff after the first, plus the sync).
const RETRY_WINDOW: Duration = Duration::from_secs(180);
/// How long `Once` is watched for a new attempt after it read `Failed`.
const ONCE_HOLD: Duration = Duration::from_secs(45);
/// The case's worst case, besides one VM's `--ready-timeout` (the recovery)
/// and one `--drain-timeout`: both retry windows and the `Once` hold.
pub const WORST_SECS: u64 = 2 * RETRY_WINDOW.as_secs() + ONCE_HOLD.as_secs();

/// The wave's VM manifest with `runStrategy` instead of `running` and the
/// SR-IOV NIC added: every start fails on the node.
pub fn failing(ctx: &Ctx, wave: usize, vm: &str, run_strategy: &str) -> Value {
    let mut m = wave::manifest(ctx, wave, vm);
    unwire(&mut m, run_strategy);
    m
}

fn unwire(m: &mut Value, run_strategy: &str) {
    let spec = m["spec"].as_object_mut().expect("manifest spec");
    spec.remove("running");
    spec.insert("runStrategy".into(), json!(run_strategy));
    let t = &mut m["spec"]["template"]["spec"];
    if let Some(ifs) = t["domain"]["devices"]["interfaces"].as_array_mut() {
        ifs.push(json!({ "name": NIC, "sriov": {} }));
    }
    if let Some(nets) = t["networks"].as_array_mut() {
        nets.push(json!({ "name": NIC, "multus": { "networkName": "stormcos-qa-unwired" } }));
    }
}

/// The merge patch that fixes it: the template's interfaces and networks
/// without the SR-IOV NIC (a merge patch replaces arrays whole).
pub fn fix_patch(failing: &Value) -> Value {
    let t = &failing["spec"]["template"]["spec"];
    let keep = |a: &Value| -> Vec<Value> { a.as_array().into_iter().flatten().filter(|x| x["name"] != NIC).cloned().collect() };
    json!({ "spec": { "template": { "spec": {
        "domain": { "devices": { "interfaces": keep(&t["domain"]["devices"]["interfaces"]) } },
        "networks": keep(&t["networks"])
    } } } })
}

/// `start failed (attempt N), retrying in Ss: …` → (N, S).
pub fn attempt(message: &str) -> Option<(u32, u64)> {
    let rest = message.split("(attempt ").nth(1)?;
    let n = rest.split(')').next()?.trim().parse().ok()?;
    let secs = rest.split("retrying in ").nth(1)?.split('s').next()?.trim().parse().ok()?;
    Some((n, secs))
}

/// One attempt as seen: its number, the delay the kubelet announced, and
/// when it was first seen (seconds since the VM was created).
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub n: u32,
    pub delay: u64,
    pub at: f64,
}

/// Is this a bounded backoff? At least 3 attempts, the announced delay
/// never shrinking, growing at least once, and never over 5 min.
pub fn backoff(seen: &[Seen]) -> Result<String, String> {
    let shown = seen.iter().map(|s| format!("#{} +{:.0}s (next {}s)", s.n, s.at, s.delay)).collect::<Vec<_>>().join(", ");
    if seen.len() < 3 {
        return Err(format!("{} attempt(s) seen: {shown}", seen.len()));
    }
    if seen.windows(2).any(|w| w[1].n <= w[0].n) {
        return Err(format!("the attempt count did not rise: {shown}"));
    }
    if seen.windows(2).any(|w| w[1].delay < w[0].delay) || seen.first().map(|s| s.delay) == seen.last().map(|s| s.delay) {
        return Err(format!("the delay did not grow: {shown}"));
    }
    if seen.iter().any(|s| s.delay > 300) {
        return Err(format!("a delay over 5 min: {shown}"));
    }
    Ok(shown)
}

/// What of the run is on the node: volumes, registrations, taps.
fn on_node(own: &Own) -> Vec<String> {
    own.volumes.iter().chain(&own.registrations).chain(&own.taps).cloned().collect()
}

/// The VM's printableStatus and its `Failure` condition's message.
async fn vm_status(ctx: &Ctx, vm: &str) -> (String, Option<String>) {
    match ctx.kube.get(&format!("{}/{vm}", kube::vms(&ctx.namespace))).await {
        Ok(r) if r.ok() => {
            let printable = r.body["status"]["printableStatus"].as_str().unwrap_or("").to_string();
            let failure = r.body["status"]["conditions"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["type"] == "Failure" && c["status"] == "True"))
                .and_then(|c| c["message"].as_str())
                .map(str::to_string);
            (printable, failure)
        }
        _ => (String::new(), None),
    }
}

async fn vmi(ctx: &Ctx, vm: &str) -> Option<Value> {
    match ctx.kube.get(&format!("{}/{vm}", kube::vmis(&ctx.namespace))).await {
        Ok(r) if r.ok() => Some(r.body),
        _ => None,
    }
}

/// Run the case for wave `wave` with the two names given (`Always`, `Once`).
/// Returns whether it failed.
pub async fn run(ctx: &Ctx, wave: usize, always: &str, once: &str) -> bool {
    let t = |s: &str| format!("wave-{wave}/failed-start/{s}");
    let failed = std::cell::Cell::new(false);
    let fail = |ctx: &Ctx, name: String, took: Duration, why: String| {
        failed.set(true);
        ctx.out.emit(Line::new(name, Status::Fail, took, why));
    };

    // Always: retried with backoff, nothing left on the node, then fixed.
    let created = Instant::now();
    let def = failing(ctx, wave, always, "Always");
    match ctx.kube.post(&kube::vms(&ctx.namespace), &def).await {
        Ok(r) if r.ok() => {
            let mut seen: Vec<Seen> = Vec::new();
            let mut last = String::from("no VMI yet");
            let mut uid = String::new();
            while created.elapsed() < RETRY_WINDOW && seen.len() < 3 {
                if let Some(v) = vmi(ctx, always).await {
                    let phase = v["status"]["phase"].as_str().unwrap_or("");
                    let reason = v["status"]["reason"].as_str().unwrap_or("");
                    let msg = v["status"]["message"].as_str().unwrap_or("");
                    uid = v["metadata"]["uid"].as_str().unwrap_or("").to_string();
                    if phase == "Running" {
                        last = "reached Running on a network nothing wires".into();
                        break;
                    }
                    if let Some((n, delay)) = attempt(msg).filter(|_| reason == "FailedStart")
                        && seen.last().is_none_or(|s| s.n != n)
                    {
                        seen.push(Seen { n, delay, at: created.elapsed().as_secs_f64() });
                    }
                    last = format!("{phase} {reason}: {msg}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let (printable, failure) = vm_status(ctx, always).await;
            match backoff(&seen) {
                Ok(shown) if printable != "Starting" && printable != "Running" && failure.is_some() => ctx.out.emit(Line::new(
                    t("retried"),
                    Status::Pass,
                    created.elapsed(),
                    format!("runStrategy Always: {shown}; VM {printable}, Failure: {}", failure.unwrap_or_default()),
                )),
                Ok(shown) => fail(
                    ctx,
                    t("retried"),
                    created.elapsed(),
                    format!("retried ({shown}) but the VM does not say it is failing: printableStatus {printable:?}, Failure condition {failure:?}"),
                ),
                Err(why) => fail(ctx, t("retried"), created.elapsed(), format!("runStrategy Always: {why}; last: {last}")),
            }

            // While it fails, every attempt has given back what it made.
            if !seen.is_empty() {
                let t0 = Instant::now();
                let left = on_node(&ctx.sources().take().await.own);
                if left.is_empty() {
                    ctx.out.emit(Line::new(t("no-residue"), Status::Pass, t0.elapsed(), format!("after {} failed attempts: no volume, registration or tap of the run", seen.last().map(|s| s.n).unwrap_or(0))));
                } else {
                    fail(ctx, t("no-residue"), t0.elapsed(), format!("while its start fails, on the node: {}", left.join(", ")));
                }
            }

            // Fixed: the NIC out of the template, the VM restarted.
            let t0 = Instant::now();
            let recovered = async {
                let path = format!("{}/{always}", kube::vms(&ctx.namespace));
                let r = ctx.kube.merge_patch(&path, &fix_patch(&def)).await?;
                anyhow::ensure!(r.ok(), "the template fix answered {}: {}", r.code, r.body);
                let r = ctx.kube.put(&kube::subresource(&ctx.namespace, always, "restart")).await?;
                anyhow::ensure!(r.ok(), "restart answered {}: {}", r.code, r.body);
                let deadline = Instant::now() + Duration::from_secs(ctx.args.ready_timeout);
                let (_, ip) = wave::wait_up(ctx, always, Some(&uid).filter(|u| !u.is_empty()).map(String::as_str), deadline).await?;
                wave::wait_ssh(ctx, &ip, deadline).await?;
                Ok(ip)
            }
            .await;
            match recovered {
                Ok(ip) => ctx.out.emit(Line::new(t("recovered"), Status::Pass, t0.elapsed(), format!("template fixed and restarted: Running at {ip}, ssh accepted the run's key"))),
                Err(e) => {
                    let ev = wave::evidence(ctx, wave, always).await;
                    failed.set(true);
                    ctx.out.failure(t("recovered"), t0.elapsed(), &e, format!("{e:#}; {ev}"));
                }
            }
        }
        other => {
            let why = match other {
                Ok(r) => format!("create answered {}: {}", r.code, r.body),
                Err(e) => format!("{e:#}"),
            };
            fail(ctx, t("retried"), created.elapsed(), why);
        }
    }

    // Once: Failed, and left so.
    let created = Instant::now();
    match ctx.kube.post(&kube::vms(&ctx.namespace), &failing(ctx, wave, once, "Once")).await {
        Ok(r) if r.ok() => {
            let mut last = String::from("no VMI yet");
            let mut ended: Option<(String, String)> = None;
            while created.elapsed() < RETRY_WINDOW && ended.is_none() {
                if let Some(v) = vmi(ctx, once).await {
                    let phase = v["status"]["phase"].as_str().unwrap_or("");
                    let msg = v["status"]["message"].as_str().unwrap_or("").to_string();
                    if phase == "Failed" {
                        ended = Some((v["metadata"]["uid"].as_str().unwrap_or("").to_string(), msg.clone()));
                    }
                    last = format!("{phase} {}: {msg}", v["status"]["reason"].as_str().unwrap_or(""));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            match ended {
                None => fail(ctx, t("once"), created.elapsed(), format!("runStrategy Once, and its VMI never went Failed: {last}")),
                Some((uid, msg)) => {
                    tokio::time::sleep(ONCE_HOLD).await;
                    let (printable, failure) = vm_status(ctx, once).await;
                    let now = vmi(ctx, once).await;
                    let same = now.as_ref().is_some_and(|v| v["metadata"]["uid"].as_str() == Some(uid.as_str()) && v["status"]["phase"] == "Failed");
                    let left = on_node(&ctx.sources().take().await.own);
                    if !same {
                        let now = now.map(|v| format!("{} {}", v["metadata"]["uid"], v["status"]["phase"])).unwrap_or_else(|| "no VMI".into());
                        fail(ctx, t("once"), created.elapsed(), format!("runStrategy Once went Failed, then was tried again within {}s (now {now})", ONCE_HOLD.as_secs()));
                    } else if printable != "Failed" || failure.is_none() {
                        fail(ctx, t("once"), created.elapsed(), format!("VMI Failed ({msg}) but the VM reads {printable:?}, Failure condition {failure:?}"));
                    } else if !left.is_empty() {
                        fail(ctx, t("once"), created.elapsed(), format!("Failed and left so, but on the node: {}", left.join(", ")));
                    } else {
                        ctx.out.emit(Line::new(
                            t("once"),
                            Status::Pass,
                            created.elapsed(),
                            format!("runStrategy Once: VMI Failed and left so for {}s, VM Failed ({}), nothing of the run on the node", ONCE_HOLD.as_secs(), failure.unwrap_or_default()),
                        ));
                    }
                }
            }
        }
        other => {
            let why = match other {
                Ok(r) => format!("create answered {}: {}", r.code, r.body),
                Err(e) => format!("{e:#}"),
            };
            fail(ctx, t("once"), created.elapsed(), why);
        }
    }

    // Both gone, nothing left.
    let t0 = Instant::now();
    let left = wave::drain(ctx, &[always.to_string(), once.to_string()]).await.unwrap_or_default();
    if left.is_empty() {
        ctx.out.emit(Line::new(t("drain"), Status::Pass, t0.elapsed(), "both deleted, nothing of the run left"));
    } else {
        fail(ctx, t("drain"), t0.elapsed(), format!("left behind after {}s: {}", ctx.args.drain_timeout, json!(left)));
    }
    failed.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_reads_the_kubelets_message() {
        assert_eq!(attempt("start failed (attempt 3), retrying in 40s: network sriov0: no pool"), Some((3, 40)));
        assert_eq!(attempt("waiting for golden fedora-44-x86_64"), None);
    }

    #[test]
    fn backoff_wants_three_rising_attempts_with_a_growing_bounded_delay() {
        let s = |n, delay, at| Seen { n, delay, at };
        assert!(backoff(&[s(1, 10, 2.0), s(2, 20, 13.0), s(3, 40, 34.0)]).is_ok());
        assert!(backoff(&[s(1, 10, 2.0), s(2, 20, 13.0)]).unwrap_err().contains("2 attempt(s)"));
        assert!(backoff(&[s(1, 10, 2.0), s(2, 10, 13.0), s(3, 10, 24.0)]).unwrap_err().contains("did not grow"));
        assert!(backoff(&[s(4, 80, 2.0), s(5, 160, 90.0), s(6, 320, 260.0)]).unwrap_err().contains("over 5 min"));
    }

    #[test]
    fn the_failing_template_adds_the_nic_and_the_fix_takes_only_it_out() {
        let mut m = json!({"spec": {"running": true, "template": {"spec": {
            "domain": {"devices": {"interfaces": [{"name": "default", "bridge": {}}]}},
            "networks": [{"name": "default", "pod": {}}]}}}});
        unwire(&mut m, "Once");
        assert_eq!(m["spec"]["runStrategy"], "Once");
        assert!(m["spec"]["running"].is_null());
        assert_eq!(m["spec"]["template"]["spec"]["domain"]["devices"]["interfaces"][1]["name"], NIC);
        assert_eq!(m["spec"]["template"]["spec"]["networks"][1]["multus"]["networkName"], "stormcos-qa-unwired");
        let p = fix_patch(&m);
        assert_eq!(p["spec"]["template"]["spec"]["domain"]["devices"]["interfaces"], json!([{"name": "default", "bridge": {}}]));
        assert_eq!(p["spec"]["template"]["spec"]["networks"], json!([{"name": "default", "pod": {}}]));
    }
}
