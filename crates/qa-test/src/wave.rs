//! One wave: ramp N VMs, hold (up → install → restart → package kept),
//! drain (all gone, nothing of ours left on the node).
//!
//! Every VM in a wave runs its steps concurrently — the failures worth
//! finding are the concurrent ones (two VMs racing for a name, a tap, a slot).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::long::Ctx;
use crate::kube;
use crate::report::{Line, Status};
use crate::{rdp, ssh};

/// One VM's timings and outcome through a wave.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VmResult {
    pub vm: String,
    /// create → VMI Running with an address.
    pub up_ms: Option<u64>,
    /// create → first ssh login: the wave's start latency.
    pub start_ms: Option<u64>,
    pub rdp: Option<bool>,
    pub package_version: Option<String>,
    /// restart request → ssh login again.
    pub restart_ms: Option<u64>,
    pub failed: bool,
}

pub fn manifest(ctx: &Ctx, wave: usize, vm: &str) -> Value {
    let mut labels = json!({ "storm.io/test-run": ctx.run_id, "storm.io/test-wave": wave.to_string() });
    labels["app.kubernetes.io/managed-by"] = json!("stormcos_qa-vm-lifecycle");
    let mut user_data = String::from("#cloud-config\n");
    if ctx.args.seed_key {
        // Diagnostic bypass for stormvm#41 only; not the definition of done.
        user_data.push_str(&format!("ssh_authorized_keys:\n  - {}\n", ctx.key.public_openssh));
    }
    let mut tspec = json!({
        "domain": {
            "cpu": { "cores": ctx.args.vm_cores },
            "memory": { "guest": format!("{}Mi", ctx.args.vm_memory_mib) },
            "resources": { "requests": { "memory": format!("{}Mi", ctx.args.vm_memory_mib) } },
            "devices": {
                // stormrdp bridges RDP to the VM's VNC socket, which exists
                // only with a graphics device.
                "autoattachGraphicsDevice": true,
                "disks": [
                    { "name": "root", "disk": { "bus": "virtio" } },
                    { "name": "seed", "disk": { "bus": "virtio" } }
                ],
                "interfaces": [ { "name": "default", "bridge": {} } ]
            }
        },
        "networks": [ { "name": "default", "pod": {} } ],
        "accessCredentials": [ {
            "sshPublicKey": {
                "source": { "secret": { "secretName": ctx.secret_name() } },
                "propagationMethod": { "noCloud": {} }
            }
        } ],
        "volumes": [
            { "name": "root", "dataVolume": { "name": ctx.args.golden } },
            { "name": "seed", "cloudInitNoCloud": { "userData": user_data } }
        ]
    });
    // Through the scheduler, not `nodeName`: a VMI placed by hand never gets
    // `status.nodeName`, and rustkube-node only watches VMIs by that field.
    if let Some(host) = &ctx.node_hostname {
        tspec["nodeSelector"] = json!({ "kubernetes.io/hostname": host });
    }
    json!({
        "apiVersion": "kubevirt.io/v1",
        "kind": "VirtualMachine",
        "metadata": { "name": vm, "namespace": ctx.namespace, "labels": labels },
        "spec": {
            "running": true,
            "template": {
                "metadata": {
                    "labels": labels,
                    // Bridged onto the node's LAN: masquerade is SLIRP on the
                    // host and nothing outside qemu can reach it (stormvm#16).
                    "annotations": { "storm.io/bridge": ctx.args.bridge }
                },
                "spec": tspec
            }
        }
    })
}

/// Wait for the VMI to be Running with an address. `not_uid`: a VMI with this
/// uid does not count (the one a restart replaced).
pub(crate) async fn wait_up(ctx: &Ctx, vm: &str, not_uid: Option<&str>, deadline: Instant) -> Result<(String, String)> {
    let path = format!("{}/{vm}", kube::vmis(&ctx.namespace));
    let mut last = String::from("no VMI yet");
    let mut failed_look = None;
    while Instant::now() < deadline {
        if let Some(r) = kube::looked(ctx.kube.get(&path).await, &mut failed_look)
            && r.ok()
        {
            let v = &r.body;
            let uid = v["metadata"]["uid"].as_str().unwrap_or_default().to_string();
            let phase = v["status"]["phase"].as_str().unwrap_or("");
            let ip = v["status"]["interfaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i["ipAddress"].as_str())
                .find(|ip| !ip.is_empty())
                .unwrap_or("");
            if not_uid == Some(uid.as_str()) {
                last = "the old VMI is still there".into();
            } else if phase == "Running" && !ip.is_empty() {
                return Ok((uid, ip.to_string()));
            } else {
                last = format!(
                    "phase {phase:?}, address {ip:?}, reason {:?}, message {:?}",
                    v["status"]["reason"].as_str().unwrap_or(""),
                    v["status"]["message"].as_str().unwrap_or("")
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(kube::timed_out(failed_look, format!("not Running with an address in time: {last}")))
}

pub(crate) async fn wait_ssh(ctx: &Ctx, ip: &str, deadline: Instant) -> Result<()> {
    let addr = format!("{ip}:22");
    let mut last = anyhow::anyhow!("never tried");
    while Instant::now() < deadline {
        match ssh::run(&addr, &ctx.args.ssh_user, ctx.key.private.clone(), "true", Duration::from_secs(20)).await {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(last.context("ssh never accepted the run's key"))
}

async fn wait_rdp(ctx: &Ctx, vm: &str, deadline: Instant) -> Result<String> {
    let mut last = anyhow::anyhow!("never tried");
    while Instant::now() < deadline {
        match rdp::probe(&ctx.args.rdp_addr(), &ctx.namespace, vm, Duration::from_secs(10)).await {
            Ok(d) => return Ok(d),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(last)
}

const VERSION_CMD: &str = "rpm -q --qf '%{VERSION}-%{RELEASE}' {p} 2>/dev/null || dpkg-query -W -f='${Version}' {p}";
const INSTALL_CMD: &str = "if command -v dnf >/dev/null; then sudo -n dnf -y -q install {p}; \
     elif command -v apt-get >/dev/null; then sudo -n apt-get -qq update && sudo -n DEBIAN_FRONTEND=noninteractive apt-get -qq -y install {p}; \
     else echo 'no dnf or apt-get in the guest'; exit 3; fi";

/// Collect a failing VM's evidence under /results: VMI, VM, events.
pub(crate) async fn evidence(ctx: &Ctx, wave: usize, vm: &str) -> String {
    let mut ev = json!({});
    for (k, p) in [
        ("vm", format!("{}/{vm}", kube::vms(&ctx.namespace))),
        ("vmi", format!("{}/{vm}", kube::vmis(&ctx.namespace))),
        ("events", format!("/api/v1/namespaces/{}/events", ctx.namespace)),
    ] {
        ev[k] = match ctx.kube.get(&p).await {
            Ok(r) if k == "events" => json!(kube::items(&r.body)
                .into_iter()
                .filter(|e| e["involvedObject"]["name"].as_str() == Some(vm))
                .collect::<Vec<_>>()),
            Ok(r) => r.body,
            Err(e) => json!(e.to_string()),
        };
    }
    let status = ev["vmi"]["status"].to_string();
    let file = ctx.results.join(format!("wave-{wave}-{vm}.json"));
    let _ = tokio::fs::write(&file, serde_json::to_vec_pretty(&ev).unwrap_or_default()).await;
    format!("VMI status {status}; evidence in {}", file.display())
}

/// Run one VM through create → up → install → restart → kept. Emits a line
/// per step. Drain is wave-wide (see [`drain`]).
pub async fn hold(ctx: Arc<Ctx>, wave: usize, vm: String, created: Instant) -> VmResult {
    let mut res = VmResult { vm: vm.clone(), ..Default::default() };
    let t = |step: &str| format!("wave-{wave}/{vm}/{step}");
    let deadline = created + Duration::from_secs(ctx.args.ready_timeout);

    // Up: Running + address, ssh login, RDP through stormrdp.
    let ip = match wait_up(&ctx, &vm, None, deadline).await {
        Ok((_, ip)) => {
            res.up_ms = Some(created.elapsed().as_millis() as u64);
            ip
        }
        Err(e) => {
            let ev = evidence(&ctx, wave, &vm).await;
            ctx.out.failure(t("up"), created.elapsed(), &e, format!("{e:#}; {ev}"));
            res.failed = true;
            return res;
        }
    };
    let rdp = wait_rdp(&ctx, &vm, deadline);
    let sshd = wait_ssh(&ctx, &ip, deadline);
    let (rdp, sshd) = tokio::join!(rdp, sshd);
    match &rdp {
        Ok(d) => ctx.out.emit(Line::new(t("rdp"), Status::Pass, created.elapsed(), d.clone())),
        Err(e) => {
            res.failed = true;
            ctx.out.emit(Line::new(t("rdp"), Status::Fail, created.elapsed(), format!("{e:#}")));
        }
    }
    res.rdp = Some(rdp.is_ok());
    if let Err(e) = sshd {
        let ev = evidence(&ctx, wave, &vm).await;
        ctx.out.emit(Line::new(t("ssh"), Status::Fail, created.elapsed(), format!("{ip}: {e:#}; {ev}")));
        res.failed = true;
        return res;
    }
    res.start_ms = Some(created.elapsed().as_millis() as u64);
    ctx.out.emit(Line::new(t("up"), Status::Pass, created.elapsed(), format!("Running at {ip}, ssh accepted the run's key")));

    // Install.
    let addr = format!("{ip}:22");
    let pkg = &ctx.args.package;
    let t0 = Instant::now();
    let cmd = |c: &str| c.replace("{p}", pkg);
    let installed = async {
        ssh::run(&addr, &ctx.args.ssh_user, ctx.key.private.clone(), &cmd(INSTALL_CMD), Duration::from_secs(ctx.args.install_timeout)).await?;
        ssh::run(&addr, &ctx.args.ssh_user, ctx.key.private.clone(), &cmd(VERSION_CMD), Duration::from_secs(30)).await
    }
    .await;
    let version = match installed {
        Ok(v) if !v.is_empty() => v,
        other => {
            let why = match other {
                Err(e) => format!("{e:#}"),
                Ok(_) => "installed but no version reported".into(),
            };
            ctx.out.emit(Line::new(t("install"), Status::Fail, t0.elapsed(), why));
            res.failed = true;
            return res;
        }
    };
    ctx.out.emit(Line::new(t("install"), Status::Pass, t0.elapsed(), format!("{pkg} {version}")));
    res.package_version = Some(version.clone());

    // "Add my keys" on the running VM (#52, rustkube-node#92).
    if !ctx.args.no_agent_keys {
        let t0 = Instant::now();
        match agent_keys(&ctx, &vm, &addr).await {
            Ok(d) => ctx.out.emit(Line::new(t("agent-keys"), Status::Pass, t0.elapsed(), d)),
            Err(e) => {
                let ev = evidence(&ctx, wave, &vm).await;
                ctx.out.failure(t("agent-keys"), t0.elapsed(), &e, format!("{e:#}; {ev}"));
                res.failed = true;
            }
        }
    }

    // Restart → up again → package kept (the root disk survived).
    let old_uid = match ctx.kube.get(&format!("{}/{vm}", kube::vmis(&ctx.namespace))).await {
        Ok(r) => r.body["metadata"]["uid"].as_str().unwrap_or_default().to_string(),
        Err(_) => String::new(),
    };
    let t0 = Instant::now();
    let restarted = async {
        let r = ctx.kube.put(&kube::subresource(&ctx.namespace, &vm, "restart")).await?;
        anyhow::ensure!(r.ok(), "restart answered {}: {}", r.code, r.body);
        let deadline = Instant::now() + Duration::from_secs(ctx.args.ready_timeout);
        let (_, ip2) = wait_up(&ctx, &vm, Some(&old_uid), deadline).await?;
        wait_ssh(&ctx, &ip2, deadline).await?;
        let v = ssh::run(&format!("{ip2}:22"), &ctx.args.ssh_user, ctx.key.private.clone(), &cmd(VERSION_CMD), Duration::from_secs(30))
            .await
            .map_err(|e| anyhow::anyhow!("{pkg} is gone after the restart (root disk re-cloned? stormvm#22): {e:#}"))?;
        anyhow::ensure!(v == version, "{pkg} was {version} before the restart and is {v:?} after");
        Ok(ip2)
    }
    .await;
    match restarted {
        Ok(ip2) => {
            res.restart_ms = Some(t0.elapsed().as_millis() as u64);
            ctx.out.emit(Line::new(t("restart"), Status::Pass, t0.elapsed(), format!("new VMI up at {ip2}, {pkg} {version} kept")));
        }
        Err(e) => {
            let ev = evidence(&ctx, wave, &vm).await;
            ctx.out.failure(t("restart"), t0.elapsed(), &e, format!("{e:#}; {ev}"));
            res.failed = true;
        }
    }
    res
}

const BOOT_ID: &str = "cat /proc/sys/kernel/random/boot_id";

/// The VMI's `accessCredentials` with the agent credential added: a key
/// Secret propagated by the guest agent to `user` (#52). A merge patch
/// replaces the array, so the ones it had are kept.
pub fn with_agent_credential(current: &Value, secret: &str, user: &str) -> Value {
    let mut creds: Vec<Value> = current.as_array().cloned().unwrap_or_default();
    creds.push(json!({ "sshPublicKey": {
        "source": { "secret": { "secretName": secret } },
        "propagationMethod": { "qemuGuestAgent": { "users": [user] } }
    } }));
    Value::Array(creds)
}

/// The VMI's `AccessCredentialsSynchronized` condition: (status, message).
pub fn access_condition(vmi: &Value) -> Option<(String, String)> {
    vmi["status"]["conditions"].as_array()?.iter().find(|c| c["type"] == "AccessCredentialsSynchronized").map(|c| {
        (c["status"].as_str().unwrap_or("").to_string(), c["message"].as_str().unwrap_or("").to_string())
    })
}

/// "Add my keys" (#52): the running VMI gets a credential whose Secret holds
/// the run's key and a second one, delivered by the guest agent. Within
/// `--agent-keys-timeout` its `AccessCredentialsSynchronized` is True and
/// ssh with the **second** key works, with the same VMI and the same boot id
/// (no restart). The VMI is patched, not the VM's template: the controller
/// copies the template only into a new VMI, and the kubelet reads the VMI as
/// it is on each sync.
async fn agent_keys(ctx: &Ctx, vm: &str, addr: &str) -> Result<String> {
    let user = &ctx.args.ssh_user;
    let boot0 = ssh::run(addr, user, ctx.key.private.clone(), BOOT_ID, Duration::from_secs(30)).await?;
    let path = format!("{}/{vm}", kube::vmis(&ctx.namespace));
    let r = ctx.kube.get(&path).await?;
    anyhow::ensure!(r.ok(), "reading the VMI answered {}: {}", r.code, r.body);
    let uid = r.body["metadata"]["uid"].as_str().unwrap_or_default().to_string();
    let creds = with_agent_credential(&r.body["spec"]["accessCredentials"], &ctx.agent_secret_name(), user);
    let t0 = Instant::now();
    let r = ctx.kube.merge_patch(&path, &json!({ "spec": { "accessCredentials": creds } })).await?;
    anyhow::ensure!(r.ok(), "adding the credential to the running VMI answered {}: {}", r.code, r.body);

    let deadline = t0 + Duration::from_secs(ctx.args.agent_keys_timeout);
    let mut last = String::from("no condition yet");
    loop {
        let vmi = ctx.kube.get(&path).await.ok().filter(|r| r.ok()).map(|r| r.body);
        if let Some(v) = &vmi {
            let now = v["metadata"]["uid"].as_str().unwrap_or_default();
            anyhow::ensure!(now == uid, "the VMI was replaced ({uid} → {now}): that is a restart");
            match access_condition(v) {
                Some((st, msg)) if st == "True" => {
                    match ssh::run(addr, user, ctx.agent_key.private.clone(), BOOT_ID, Duration::from_secs(30)).await {
                        Ok(boot) => {
                            anyhow::ensure!(boot == boot0, "the second key works, but the guest rebooted (boot id {boot0} → {boot})");
                            return Ok(format!(
                                "second key added through the guest agent in {:.0} s: AccessCredentialsSynchronized True, ssh with it accepted, same VMI and boot id (no restart)",
                                t0.elapsed().as_secs_f64()
                            ));
                        }
                        Err(e) => last = format!("condition True ({msg}), ssh with the second key: {e:#}"),
                    }
                }
                Some((st, msg)) => last = format!("AccessCredentialsSynchronized {st}: {msg}"),
                None => {}
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("not in {}s: {last}", ctx.args.agent_keys_timeout);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Delete every VM of the run, then wait until the VMs, VMIs and the run's
/// volumes, registrations and taps are all gone. Returns what is left.
pub async fn drain(ctx: &Ctx, names: &[String]) -> Result<crate::census::Own> {
    for vm in names {
        let _ = ctx.kube.delete(&format!("{}/{vm}", kube::vms(&ctx.namespace))).await;
    }
    let deadline = Instant::now() + Duration::from_secs(ctx.args.drain_timeout);
    loop {
        let own = ctx.sources().take().await.own;
        if own.is_empty() || Instant::now() >= deadline {
            return Ok(own);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_credential_is_added_to_the_ones_the_vmi_has() {
        let boot = json!([{"sshPublicKey": {"source": {"secret": {"secretName": "k"}}, "propagationMethod": {"noCloud": {}}}}]);
        let c = with_agent_credential(&boot, "k-agent", "fedora");
        assert_eq!(c[0], boot[0]);
        assert_eq!(c[1]["sshPublicKey"]["source"]["secret"]["secretName"], "k-agent");
        assert_eq!(c[1]["sshPublicKey"]["propagationMethod"]["qemuGuestAgent"]["users"], json!(["fedora"]));
        assert_eq!(with_agent_credential(&Value::Null, "k-agent", "fedora").as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn access_condition_is_found_by_type() {
        let v = json!({"status": {"conditions": [{"type": "Ready", "status": "True"},
            {"type": "AccessCredentialsSynchronized", "status": "False", "message": "waiting for the guest agent to answer"}]}});
        assert_eq!(access_condition(&v), Some(("False".into(), "waiting for the guest agent to answer".into())));
        assert_eq!(access_condition(&json!({})), None);
    }
}
