//! `/test podapi` — what a container can do with the Kubernetes API, the way
//! an OpenShift or Kubernetes one can (#82). Every item is checked from inside
//! a pod, on the `default` ServiceAccount, not from the Job's own `storm-test`.
//!
//! The driver (the Job) starts a long-lived `/test podapi-probe` pod. The probe
//! reports over its log, one JSON line per fact with no spaces in it, and keeps
//! polling what it may do and holding a watch, so the driver only changes the
//! world (a Role, a marker pod, deleting the pod) and reads what the pod saw.
//! Token claims are logged without the signature; **the tokens themselves are
//! never logged**: the driver needs them for a TokenReview and to try one after
//! its pod is gone, and fetches them over TCP from the pod.
//!
//! - `mounted/files`, `env/set`, `env/service-ip`: items 1 and 2;
//! - `reachable/dns`, `reachable/by-name`, `reachable/by-service-ip`: item 3,
//!   TLS verified against the mounted `ca.crt` alone;
//! - `authenticated/api`, `authenticated/whoami`: item 4;
//! - `rbac/refused`, `rbac/granted`, `rbac/revoked`: item 5;
//! - `bound/projected-token`, `bound/rotated`, `bound/rotated-default`,
//!   `bound/refused-after-delete`: item 6;
//! - `optout/pod`, `optout/sa`: item 7;
//! - `projected/custom-token`, `projected/token-review`,
//!   `projected/other-audience-refused`: item 8;
//! - `downward/env`, `downward/volume`: item 9;
//! - `watch/stream`: item 10;
//! - `openshift/*`: item 11, recorded: pass where we have it, skip where not.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent::SERVE_PORT;
use crate::kube::{self, Client};
use crate::report::{Line, Out, Status};

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const API_NAME: &str = "kubernetes.default.svc";
/// The custom projection (item 8): its own mount, audience and lifetime. 600 s
/// is the shortest the apiserver takes, and rotates at 80%: 480 s, in a day run.
const CUSTOM_DIR: &str = "/var/run/podapi/custom";
const AUDIENCE: &str = "stormcos-qa-podapi";
const CUSTOM_TTL: i64 = 600;
/// The default projection's lifetime, as upstream's kubelet asks for it.
const DEFAULT_TTL: i64 = 3607;
const INFO_DIR: &str = "/var/run/podapi/info";
const LIMIT_MEMORY: &str = "256Mi";
const LIMIT_MEMORY_BYTES: &str = "268435456";
/// 500m with the default divisor of 1 is rounded up, as upstream.
const LIMIT_CPU: &str = "500m";
const LIMIT_CPU_CORES: &str = "1";
/// The ServiceAccount with `create tokenreviews` (requires.toml).
const REVIEWER: &str = "pa-reviewer";
const NO_AUTOMOUNT: &str = "pa-noauto";
/// How long the probe lives, past any budget this suite declares.
const PROBE_LIFE: Duration = Duration::from_secs(1800);

/// One line per item, so the matrix keeps its shape when a run cannot start.
/// Not `openshift/*`: those are recorded when asked, and never read as a gap.
const ITEMS: [&str; 22] = [
    "mounted/files",
    "env/set",
    "env/service-ip",
    "reachable/dns",
    "reachable/by-name",
    "reachable/by-service-ip",
    "authenticated/api",
    "authenticated/whoami",
    "rbac/refused",
    "rbac/granted",
    "rbac/revoked",
    "bound/projected-token",
    "bound/rotated",
    "bound/refused-after-delete",
    "optout/pod",
    "optout/sa",
    "projected/custom-token",
    "projected/token-review",
    "projected/other-audience-refused",
    "downward/env",
    "downward/volume",
    "watch/stream",
];

#[derive(Parser, Debug)]
#[command(name = "test podapi", about = "In-pod Kubernetes API parity with OpenShift (#82)")]
pub struct Args {
    #[arg(long, env = "STORM_API", default_value = "")]
    api: String,
    #[arg(long)]
    token_file: Option<String>,
    #[arg(long)]
    insecure: bool,
    #[arg(long, env = "STORM_NAMESPACE")]
    namespace: Option<String>,
    #[arg(long, env = "STORM_RUN_ID", default_value = "manual")]
    run_id: String,
    #[arg(long, env = "STORM_RESULTS", default_value = "/results")]
    results: std::path::PathBuf,
    /// The run's budget: the rotation wait is cut to fit it.
    #[arg(long, env = "STORM_TIMEOUT", default_value_t = 900)]
    timeout: u64,
    /// This test's image (default: the Job pod's own).
    #[arg(long)]
    image: Option<String>,
    /// How long a pod may take to run, or to report.
    #[arg(long, default_value_t = 180)]
    pod_timeout: u64,
    /// "Within seconds" (item 5): how long a grant or a removal may take.
    #[arg(long, default_value_t = 30)]
    rbac_within: u64,
    /// How long the watch must stay open before its last event (item 10).
    #[arg(long, default_value_t = 60)]
    watch_hold: u64,
    /// How long after the probe starts its 600 s token may take to rotate.
    #[arg(long, default_value_t = 600)]
    rotation_timeout: u64,
}

// ---- the pod's log ----

fn say(v: Value) {
    println!("{}", crate::report::unspaced(&v.to_string()));
}

/// The probe's lines in a pod log; anything else (a runtime's warning) is not.
pub fn parse(log: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(&l.trim().replace('\u{b7}', " ")).ok())
        .filter(|v| v["pa"].is_string())
        .collect()
}

fn find<'a>(lines: &'a [Value], pa: &str) -> Option<&'a Value> {
    lines.iter().find(|v| v["pa"] == pa)
}

fn found<'a>(lines: &'a [Value], pa: &str, via: &str) -> Option<&'a Value> {
    lines.iter().find(|v| v["pa"] == pa && v["via"] == via)
}

fn token_claims<'a>(lines: &'a [Value], vol: &str) -> Vec<&'a Value> {
    lines.iter().filter(|v| v["pa"] == "token" && v["vol"] == vol).collect()
}

// ---- the judges ----

type Verdict = (Status, String);

fn pass(d: impl Into<String>) -> Verdict {
    (Status::Pass, d.into())
}

fn fail(d: impl Into<String>) -> Verdict {
    (Status::Fail, d.into())
}

fn silent(what: &str) -> Verdict {
    fail(format!("the probe reported nothing for {what}"))
}

/// Item 1: the three files, and `namespace` is the pod's.
pub fn judge_mounted(lines: &[Value], ns: &str) -> Verdict {
    let Some(m) = find(lines, "mounted") else { return silent("the mounted files") };
    let mut why = Vec::new();
    if m["token"] != true {
        why.push("no token".to_string());
    }
    if m["ca"] != true {
        why.push("no ca.crt".to_string());
    }
    match m["namespace"].as_str() {
        None => why.push("no namespace".to_string()),
        Some(n) if n != ns => why.push(format!("namespace says {n:?}, the pod is in {ns:?}")),
        Some(_) => {}
    }
    if why.is_empty() { pass(format!("{SA}: token, ca.crt, namespace = {ns}")) } else { fail(format!("{SA}: {}", why.join("; "))) }
}

/// Item 2: the three variables are set.
pub fn judge_env(lines: &[Value]) -> Verdict {
    let Some(e) = find(lines, "env") else { return silent("the environment") };
    let names = ["KUBERNETES_SERVICE_HOST", "KUBERNETES_SERVICE_PORT", "KUBERNETES_SERVICE_PORT_HTTPS"];
    let unset: Vec<&str> = names.into_iter().filter(|n| e[*n].as_str().is_none_or(str::is_empty)).collect();
    if unset.is_empty() {
        pass(format!("{}:{} (https {})", e[names[0]].as_str().unwrap_or(""), e[names[1]].as_str().unwrap_or(""), e[names[2]].as_str().unwrap_or("")))
    } else {
        fail(format!("unset: {}", unset.join(" ")))
    }
}

/// Item 2's values, as a client sees them: on Kubernetes and OpenShift they name
/// the `kubernetes` Service, not an apiserver behind it.
pub fn judge_env_service(lines: &[Value], ip: &str, port: &str) -> Verdict {
    let Some(e) = find(lines, "env") else { return silent("the environment") };
    let host = e["KUBERNETES_SERVICE_HOST"].as_str().unwrap_or("");
    let got = e["KUBERNETES_SERVICE_PORT"].as_str().unwrap_or("");
    if host == ip && got == port {
        pass(format!("{host}:{got} is the kubernetes Service"))
    } else {
        fail(format!("{host}:{got}, the kubernetes Service is {ip}:{port}"))
    }
}

/// Item 3: the short name resolves in the pod, to the Service.
pub fn judge_dns(lines: &[Value], ip: &str) -> Verdict {
    let Some(d) = find(lines, "dns") else { return silent("DNS") };
    let addrs: Vec<&str> = d["addrs"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if d["ok"] != true {
        return fail(format!("{API_NAME} did not resolve: {} (nameservers {})", d["why"].as_str().unwrap_or("?"), d["nameservers"]));
    }
    if addrs.contains(&ip) { pass(format!("{API_NAME} -> {}", addrs.join(","))) } else { fail(format!("{API_NAME} -> {}, the kubernetes Service is {ip}", addrs.join(","))) }
}

/// Item 3: TLS verified against the mounted `ca.crt` alone, and an answer.
pub fn judge_https(lines: &[Value], via: &str) -> Verdict {
    let Some(h) = found(lines, "https", via) else { return silent(&format!("https by {via}")) };
    let url = h["url"].as_str().unwrap_or("?");
    match h["code"].as_u64() {
        Some(c) if c < 500 => pass(format!("{url} answered {c}, verified against only the mounted ca.crt")),
        Some(c) => fail(format!("{url} answered {c}")),
        None => fail(format!("{url}: {}", h["err"].as_str().unwrap_or("?"))),
    }
}

/// Item 4: `GET /api` with the token.
pub fn judge_api(lines: &[Value]) -> Verdict {
    let Some(a) = find(lines, "api") else { return silent("GET /api") };
    if a["code"] == 200 && a["kind"] == "APIVersions" { pass(format!("GET /api: 200 APIVersions, through {}", a["base"].as_str().unwrap_or("?"))) } else { fail(format!("GET /api: code {} kind {} {}", a["code"], a["kind"], a["err"].as_str().unwrap_or(""))) }
}

/// Item 4: a SelfSubjectReview names the ServiceAccount and its groups.
pub fn judge_whoami(lines: &[Value], ns: &str, sa: &str) -> Verdict {
    let Some(w) = find(lines, "whoami") else { return silent("the SelfSubjectReview") };
    let user = w["username"].as_str().unwrap_or("");
    let groups: Vec<&str> = w["groups"].as_array().map(|g| g.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let want = format!("system:serviceaccount:{ns}:{sa}");
    let ns_group = format!("system:serviceaccounts:{ns}");
    let missing: Vec<&str> = ["system:serviceaccounts", ns_group.as_str(), "system:authenticated"].into_iter().filter(|g| !groups.contains(g)).collect();
    if !matches!(w["code"].as_u64(), Some(200 | 201)) {
        fail(format!("SelfSubjectReview: code {} {}", w["code"], w["err"].as_str().unwrap_or("")))
    } else if user != want {
        fail(format!("names {user:?}, want {want:?}"))
    } else if !missing.is_empty() {
        fail(format!("{user}: missing groups {} (has {})", missing.join(" "), groups.join(" ")))
    } else {
        pass(format!("{user} in {}", groups.join(" ")))
    }
}

fn claim_i64(c: &Value, k: &str) -> i64 {
    c[k].as_i64().unwrap_or(0)
}

/// What a token's claims say about the pod it is bound to.
fn bound_to(c: &Value, pod: &str, uid: &str) -> std::result::Result<(), String> {
    let got = (c["pod"]["name"].as_str().unwrap_or(""), c["pod"]["uid"].as_str().unwrap_or(""));
    if got == (pod, uid) { Ok(()) } else { Err(format!("bound to pod {:?} uid {:?}, this pod is {pod:?} uid {uid:?}", got.0, got.1)) }
}

/// Item 6: the default token is bound to this pod, carries audiences, and lasts
/// about an hour. An apiserver that extends a pod's 3607 s token (upstream's
/// default) gives `exp` a year on and `warnafter` the hour: that is the hour.
pub fn judge_default_token(lines: &[Value], pod: &str, uid: &str, now: i64) -> Verdict {
    let Some(c) = token_claims(lines, "default").first().map(|l| &l["claims"]) else { return silent("the default token") };
    if let Err(why) = bound_to(c, pod, uid) {
        return fail(why);
    }
    let (iat, exp, warn) = (claim_i64(c, "iat"), claim_i64(c, "exp"), claim_i64(c, "warnafter"));
    let aud = c["aud"].as_array().map(Vec::len).unwrap_or(0);
    let life = if warn > 0 { warn - iat } else { exp - iat };
    if aud == 0 {
        fail("no audience in the token")
    } else if exp <= now {
        fail(format!("expired: exp {exp}, now {now}"))
    } else if !(DEFAULT_TTL - 300..=DEFAULT_TTL + 300).contains(&life) {
        fail(format!("lasts {life} s (exp {} s, warnafter {}), want about {DEFAULT_TTL}", exp - iat, if warn > 0 { warn - iat } else { 0 }))
    } else if warn > 0 {
        pass(format!("bound to {pod}; refresh by {life} s (warnafter); exp {} s on, extended as upstream", exp - iat))
    } else {
        pass(format!("bound to {pod}; exp {life} s on"))
    }
}

/// Item 6: the mount at the ServiceAccount path is a projected token volume
/// (the apiserver's `kube-api-access-*`), not a kubelet's fallback.
pub fn has_token_projection(pod: &Value) -> bool {
    let Some(mounts) = pod["spec"]["containers"][0]["volumeMounts"].as_array() else { return false };
    let Some(name) = mounts.iter().find(|m| m["mountPath"] == SA).and_then(|m| m["name"].as_str()) else { return false };
    pod["spec"]["volumes"].as_array().is_some_and(|vs| {
        vs.iter().any(|v| v["name"] == name && v["projected"]["sources"].as_array().is_some_and(|ss| ss.iter().any(|s| s["serviceAccountToken"].is_object())))
    })
}

/// Item 8: the custom projection has its audience and its lifetime.
pub fn judge_custom_token(lines: &[Value], pod: &str, uid: &str) -> Verdict {
    let Some(c) = token_claims(lines, "custom").first().map(|l| &l["claims"]) else { return silent("the custom token") };
    if let Err(why) = bound_to(c, pod, uid) {
        return fail(why);
    }
    let aud: Vec<&str> = c["aud"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let life = claim_i64(c, "exp") - claim_i64(c, "iat");
    if aud != [AUDIENCE] {
        fail(format!("audience {aud:?}, asked for {AUDIENCE:?}"))
    } else if life != CUSTOM_TTL {
        fail(format!("lasts {life} s, asked for {CUSTOM_TTL}"))
    } else {
        pass(format!("audience {AUDIENCE}, {life} s, bound to {pod}"))
    }
}

/// Item 6's rotation: the file changed to a later token before the first ended.
pub fn judge_rotated(lines: &[Value]) -> Verdict {
    let tokens = token_claims(lines, "custom");
    let Some(first) = tokens.first().map(|l| &l["claims"]) else { return silent("the custom token") };
    let Some(next) = tokens.iter().map(|l| &l["claims"]).find(|c| c["jti"] != first["jti"]) else {
        return fail(format!("the {CUSTOM_TTL} s token was not rewritten in the time given (due at 80%: {} s)", CUSTOM_TTL * 4 / 5));
    };
    let (born, ended, new) = (claim_i64(first, "iat"), claim_i64(first, "exp"), claim_i64(next, "iat"));
    if new >= ended {
        fail(format!("rewritten {} s on, after it expired at {} s", new - born, ended - born))
    } else {
        pass(format!("rewritten on disk {} s into its {} s", new - born, ended - born))
    }
}

/// Item 9: the variables match what the API says of this pod.
pub fn judge_downward_env(lines: &[Value], pod: &Value) -> Verdict {
    let Some(d) = find(lines, "downward-env") else { return silent("the downward env") };
    let want = [
        ("POD_NAME", pod["metadata"]["name"].as_str().unwrap_or("?")),
        ("POD_NAMESPACE", pod["metadata"]["namespace"].as_str().unwrap_or("?")),
        ("POD_IP", pod["status"]["podIP"].as_str().unwrap_or("?")),
        ("NODE_NAME", pod["spec"]["nodeName"].as_str().unwrap_or("?")),
        ("LIMIT_MEMORY", LIMIT_MEMORY_BYTES),
        ("LIMIT_CPU", LIMIT_CPU_CORES),
    ];
    let bad: Vec<String> = want.iter().filter(|(k, v)| d[*k].as_str() != Some(*v)).map(|(k, v)| format!("{k} is {}, want {v}", d[*k])).collect();
    if bad.is_empty() { pass(want.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")) } else { fail(bad.join("; ")) }
}

/// Item 9: the volume's files hold the pod's labels and annotations, each
/// `key="value"` on a line.
pub fn judge_downward_volume(lines: &[Value], id: &str) -> Verdict {
    let Some(d) = find(lines, "downward-volume") else { return silent("the downward volume") };
    let mut bad = Vec::new();
    for (file, want) in [("labels", format!("storm.io/podapi=\"{id}\"")), ("annotations", format!("storm.io/podapi-note=\"{id}\""))] {
        match d[file].as_str() {
            Some(text) if text.lines().any(|l| l.trim() == want) => {}
            Some(text) => bad.push(format!("{INFO_DIR}/{file} has no {want}: {text:?}")),
            None => bad.push(format!("{INFO_DIR}/{file}: {}", d[format!("{file}_err")].as_str().unwrap_or("?"))),
        }
    }
    if bad.is_empty() { pass(format!("{INFO_DIR}: labels and annotations filled")) } else { fail(bad.join("; ")) }
}

/// Item 7: a pod opted out has no token, and ended on its own.
pub fn judge_optout(lines: &[Value], phase: &str) -> Verdict {
    let Some(m) = find(lines, "mounted") else { return fail(format!("the pod reported nothing (phase {phase:?})")) };
    if m["token"] == true {
        fail(format!("a token is mounted at {SA}/token"))
    } else {
        pass(format!("no token at {SA}"))
    }
}

/// The probe's RBAC answers now: GET a pod, LIST Secrets.
fn rbac_now(lines: &[Value]) -> Option<(u64, u64)> {
    let l = lines.iter().rfind(|v| v["pa"] == "rbac")?;
    Some((l["get_pods"].as_u64().unwrap_or(0), l["list_secrets"].as_u64().unwrap_or(0)))
}

/// Item 10: the marker's two events on one connection that never ended.
pub fn judge_watch(lines: &[Value], marker: &str, held: Duration) -> Verdict {
    let saw = |ty: &str| lines.iter().any(|v| v["pa"] == "event" && v["type"] == ty && v["name"] == marker);
    let opens = lines.iter().filter(|v| v["pa"] == "watch" && v["state"] == "open").count();
    let ended = lines.iter().filter(|v| v["pa"] == "watch" && v["state"] == "ended").count();
    let via = lines.iter().find(|v| v["pa"] == "watch" && v["state"] == "open").and_then(|v| v["base"].as_str()).unwrap_or("?");
    if opens == 0 {
        let last = lines.iter().rfind(|v| v["pa"] == "watch");
        return fail(format!("the watch never opened; last: {}", last.map_or("nothing".to_string(), |v| format!("{} {}", v["state"], v["why"]))));
    }
    let mut bad = Vec::new();
    if !saw("ADDED") {
        bad.push("no ADDED".to_string());
    }
    if !saw("DELETED") {
        bad.push("no DELETED".to_string());
    }
    if ended > 0 || opens > 1 {
        bad.push(format!("the connection did not last: {opens} opened, {ended} ended"));
    }
    if bad.is_empty() {
        pass(format!("ADDED and DELETED of {marker} on one connection through {via}, held {} s", held.as_secs()))
    } else {
        fail(format!("{marker}: {}", bad.join("; ")))
    }
}

/// Item 8: the review of the custom token, for its audience. (A TokenReview's
/// answer may echo the token: only its status is ever read.)
pub fn judge_review(r: &kube::Resp, ns: &str) -> Verdict {
    let st = &r.body["status"];
    let want = format!("system:serviceaccount:{ns}:default");
    if !matches!(r.code, 200 | 201) {
        return fail(format!("TokenReview answered {}", r.code));
    }
    let auds: Vec<&str> = st["audiences"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if st["authenticated"] != true {
        fail(format!("not authenticated for {AUDIENCE}: {}", st["error"]))
    } else if !auds.contains(&AUDIENCE) {
        fail(format!("status.audiences {auds:?} lack {AUDIENCE}"))
    } else if st["user"]["username"].as_str() != Some(want.as_str()) {
        fail(format!("names {}, want {want}", st["user"]["username"]))
    } else {
        pass(format!("authenticated for {AUDIENCE} as {want}"))
    }
}

// ---- the tokens, over TCP ----

/// The pod's two tokens. No Debug: nothing can print one by accident.
struct Tokens {
    default: String,
    custom: String,
}

async fn fetch_tokens(addr: &str) -> Result<Tokens> {
    let (r, _) = retry::probe(&retry::Policy::PROBE, &format!("tokens from {addr}"), |_| async {
        let go = async {
            let mut c = tokio::net::TcpStream::connect(addr).await?;
            let mut buf = String::new();
            c.read_to_string(&mut buf).await?;
            std::io::Result::Ok(buf)
        };
        let r = tokio::time::timeout(Duration::from_secs(5), go).await;
        let ok = matches!(&r, Ok(Ok(b)) if !b.is_empty());
        (r, ok)
    })
    .await;
    let text = r.context("no answer in 5 s")?.with_context(|| format!("connecting to {addr}"))?;
    // The error names no content: this text is tokens.
    let v: Value = serde_json::from_str(&text).map_err(|_| anyhow!("the pod's answer is not JSON"))?;
    Ok(Tokens { default: v["default"].as_str().unwrap_or("").to_string(), custom: v["custom"].as_str().unwrap_or("").to_string() })
}

// ---- the driver ----

struct Driver<'a> {
    kube: &'a Client,
    ns: String,
    run: String,
    id: String,
    image: String,
    pod_timeout: Duration,
    out: &'a Out,
}

impl Driver<'_> {
    fn pods(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.ns)
    }

    fn emit(&self, test: &str, t: Instant, (st, d): Verdict) {
        self.out.emit(Line::new(test, st, t.elapsed(), d));
    }

    async fn create(&self, path: &str, body: &Value) -> Result<()> {
        let r = self.kube.post(path, body).await?;
        if !r.ok() {
            return Err(anyhow!("creating {} answered {}: {}", body["metadata"]["name"], r.code, r.body));
        }
        Ok(())
    }

    /// The probe: the `default` ServiceAccount, nothing set against automount.
    fn probe_pod(&self, name: &str, service_ip: &str, service_port: &str) -> Value {
        let field = |p: &str| json!({"valueFrom": {"fieldRef": {"fieldPath": p}}});
        let with = |n: &str, mut v: Value| {
            v["name"] = json!(n);
            v
        };
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": name,
                            "labels": {"storm.io/test-run": self.run, "storm.io/podapi": self.id},
                            "annotations": {"storm.io/podapi-note": self.id}},
               "spec": {"restartPolicy": "Never", "terminationGracePeriodSeconds": 1,
                        "volumes": [
                            {"name": "custom", "projected": {"sources": [{"serviceAccountToken": {"audience": AUDIENCE, "expirationSeconds": CUSTOM_TTL, "path": "token"}}]}},
                            {"name": "info", "downwardAPI": {"items": [
                                {"path": "labels", "fieldRef": {"fieldPath": "metadata.labels"}},
                                {"path": "annotations", "fieldRef": {"fieldPath": "metadata.annotations"}}]}}],
                        "containers": [{"name": "c", "image": self.image, "command": ["/test"], "args": ["podapi-probe"],
                            "resources": {"requests": {"cpu": "100m", "memory": LIMIT_MEMORY}, "limits": {"cpu": LIMIT_CPU, "memory": LIMIT_MEMORY}},
                            "volumeMounts": [{"name": "custom", "mountPath": CUSTOM_DIR, "readOnly": true}, {"name": "info", "mountPath": INFO_DIR, "readOnly": true}],
                            "env": [
                                with("POD_NAME", field("metadata.name")),
                                with("POD_NAMESPACE", field("metadata.namespace")),
                                with("POD_IP", field("status.podIP")),
                                with("NODE_NAME", field("spec.nodeName")),
                                {"name": "LIMIT_MEMORY", "valueFrom": {"resourceFieldRef": {"resource": "limits.memory"}}},
                                {"name": "LIMIT_CPU", "valueFrom": {"resourceFieldRef": {"resource": "limits.cpu"}}},
                                {"name": "PA_SERVICE_IP", "value": service_ip},
                                {"name": "PA_SERVICE_PORT", "value": service_port},
                            ]}]}})
    }

    async fn pod_state(&self, name: &str) -> Result<kube::Resp> {
        self.kube.get(&format!("{}/{name}", self.pods())).await
    }

    /// Wait for the pod to reach `done`, or the timeout: its last state.
    async fn wait_pod(&self, name: &str, done: fn(&Value) -> bool) -> Result<Value> {
        let deadline = Instant::now() + self.pod_timeout;
        loop {
            let r = self.pod_state(name).await?;
            if done(&r.body) || Instant::now() >= deadline {
                return Ok(r.body);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn lines(&self, pod: &str) -> Vec<Value> {
        let got = self.kube.text(&format!("{}/{pod}/log", self.pods())).await;
        got.map(|(_, t)| parse(&t)).unwrap_or_default()
    }

    /// Read the pod's log until `pred` holds or `within` runs out: the last
    /// lines, and whether it held.
    async fn wait_lines(&self, pod: &str, within: Duration, pred: impl Fn(&[Value]) -> bool) -> (Vec<Value>, bool) {
        let deadline = Instant::now() + within;
        loop {
            let lines = self.lines(pod).await;
            if pred(&lines) {
                return (lines, true);
            }
            if Instant::now() >= deadline {
                return (lines, false);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn delete_rbac(&self, name: &str) {
        let base = format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}", self.ns);
        let _ = self.kube.delete(&format!("{base}/rolebindings/{name}")).await;
        let _ = self.kube.delete(&format!("{base}/roles/{name}")).await;
    }

    /// A Role and a RoleBinding to the `default` ServiceAccount.
    async fn grant(&self, name: &str, verbs: &[&str]) -> Result<()> {
        let base = format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}", self.ns);
        let labels = json!({"storm.io/test-run": self.run});
        self.create(&format!("{base}/roles"), &json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": name, "labels": labels},
            "rules": [{"apiGroups": [""], "resources": ["pods"], "verbs": verbs}]}))
            .await?;
        self.create(&format!("{base}/rolebindings"), &json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": name, "labels": labels},
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name},
            "subjects": [{"kind": "ServiceAccount", "name": "default", "namespace": self.ns}]}))
            .await
    }

    /// Item 5, as the pod sees it.
    async fn rbac(&self, probe: &str, within: Duration) {
        let t = Instant::now();
        let (lines, _) = self.wait_lines(probe, self.pod_timeout, |l| rbac_now(l).is_some()).await;
        let verdict = match rbac_now(&lines) {
            None => silent("its RBAC answers"),
            Some((403, 403)) => pass("LIST Secrets 403, GET a pod 403, as the default ServiceAccount"),
            Some((p, s)) => fail(format!("as the default ServiceAccount, LIST Secrets {s} and GET a pod {p}; want both 403")),
        };
        self.emit("rbac/refused", t, verdict);

        let t = Instant::now();
        let role = format!("pa-get-{}", self.id);
        if let Err(e) = self.grant(&role, &["get"]).await {
            self.out.failure("rbac/granted", t.elapsed(), &e, format!("{e:#}"));
            self.out.could_not_run("rbac/revoked", t.elapsed(), "nothing was granted");
            self.delete_rbac(&role).await;
            return;
        }
        let (lines, took) = self.wait_lines(probe, within, |l| rbac_now(l).is_some_and(|(p, _)| p == 200)).await;
        let state = rbac_now(&lines);
        let verdict = match state {
            Some((200, 403)) if took => pass(format!("GET a pod 200 {} s after the Role and RoleBinding; LIST Secrets still 403", t.elapsed().as_secs())),
            Some((200, s)) if took => fail(format!("a Role granting only `get pods` made LIST Secrets {s}")),
            other => fail(format!("not 200 within {} s: GET a pod, LIST Secrets {other:?}", within.as_secs())),
        };
        self.emit("rbac/granted", t, verdict);

        let t = Instant::now();
        self.delete_rbac(&role).await;
        let (lines, took) = self.wait_lines(probe, within, |l| rbac_now(l).is_some_and(|(p, _)| p == 403)).await;
        let verdict = if took {
            pass(format!("GET a pod 403 again {} s after the Role and RoleBinding were removed", t.elapsed().as_secs()))
        } else {
            fail(format!("not 403 within {} s of the removal: GET a pod, LIST Secrets {:?}", within.as_secs(), rbac_now(&lines)))
        };
        self.emit("rbac/revoked", t, verdict);
    }

    /// Item 10: the probe's watch, once it may; a marker pod's life on it.
    async fn watch(&self, probe: &str, within: Duration, hold: Duration) {
        let t = Instant::now();
        let role = format!("pa-watch-{}", self.id);
        if let Err(e) = self.grant(&role, &["list", "watch"]).await {
            self.out.failure("watch/stream", t.elapsed(), &e, format!("{e:#}"));
            self.delete_rbac(&role).await;
            return;
        }
        let open = |l: &[Value]| l.iter().any(|v| v["pa"] == "watch" && v["state"] == "open");
        let (lines, opened) = self.wait_lines(probe, within, open).await;
        if !opened {
            self.emit("watch/stream", t, judge_watch(&lines, "", hold));
            self.delete_rbac(&role).await;
            return;
        }
        let marker = format!("pa-mark-{}", self.id);
        let body = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": marker, "labels": {"storm.io/test-run": self.run}},
            "spec": {"restartPolicy": "Never", "terminationGracePeriodSeconds": 1, "automountServiceAccountToken": false,
                     "containers": [{"name": "c", "image": self.image, "command": ["/test"], "args": ["serve"]}]}});
        if let Err(e) = self.create(&self.pods(), &body).await {
            self.out.failure("watch/stream", t.elapsed(), &e, format!("{e:#}"));
            self.delete_rbac(&role).await;
            return;
        }
        // A last event after the hold: a stream that went quiet would miss it.
        tokio::time::sleep(hold).await;
        let _ = self.kube.delete(&format!("{}/{marker}", self.pods())).await;
        let (lines, _) = self
            .wait_lines(probe, self.pod_timeout, |l: &[Value]| l.iter().any(|v| v["pa"] == "event" && v["type"] == "DELETED" && v["name"] == marker.as_str()))
            .await;
        self.emit("watch/stream", t, judge_watch(&lines, &marker, hold));
        self.delete_rbac(&role).await;
    }

    /// Item 7: pods that report their mount and end.
    async fn optout(&self) {
        let t = Instant::now();
        let sa = json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": NO_AUTOMOUNT, "labels": {"storm.io/test-run": self.run}}, "automountServiceAccountToken": false});
        let sa_made = self.create(&format!("/api/v1/namespaces/{}/serviceaccounts", self.ns), &sa).await;
        for (test, name, spec) in [
            ("optout/pod", format!("pa-noauto-pod-{}", self.id), json!({"automountServiceAccountToken": false})),
            ("optout/sa", format!("pa-noauto-sa-{}", self.id), json!({"serviceAccountName": NO_AUTOMOUNT})),
        ] {
            if test == "optout/sa"
                && let Err(e) = &sa_made
            {
                self.out.failure(test, t.elapsed(), e, format!("{e:#}"));
                continue;
            }
            let t = Instant::now();
            let mut spec = spec;
            spec["restartPolicy"] = json!("Never");
            spec["terminationGracePeriodSeconds"] = json!(1);
            spec["containers"] = json!([{"name": "c", "image": self.image, "command": ["/test"], "args": ["podapi-probe", "--opt-out"]}]);
            let body = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": {"storm.io/test-run": self.run}}, "spec": spec});
            let checked = async {
                self.create(&self.pods(), &body).await?;
                let p = self.wait_pod(&name, ended).await?;
                anyhow::Ok(p["status"]["phase"].as_str().unwrap_or("?").to_string())
            }
            .await;
            match checked {
                Ok(phase) => self.emit(test, t, judge_optout(&self.lines(&name).await, &phase)),
                Err(e) => self.out.failure(test, t.elapsed(), &e, format!("{e:#}")),
            }
        }
    }

    /// Item 8's review, and the apiserver's own answer to that audience.
    async fn review(&self, tokens: &Tokens) {
        let t = Instant::now();
        let reviews = "/apis/authentication.k8s.io/v1/tokenreviews";
        let path = format!("/api/v1/namespaces/{}/serviceaccounts/{REVIEWER}/token", self.ns);
        let issued = self.kube.post_once(&path, &json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": {}})).await;
        let reviewer = match issued {
            Ok(r) if r.ok() && r.body["status"]["token"].is_string() => r.body["status"]["token"].as_str().unwrap_or("").to_string(),
            Ok(r) => {
                self.out.could_not_run("projected/token-review", t.elapsed(), format!("a token for {REVIEWER} answered {}", r.code));
                String::new()
            }
            Err(e) => {
                self.out.failure("projected/token-review", t.elapsed(), &e, format!("a token for {REVIEWER}: {e:#}"));
                String::new()
            }
        };
        if !reviewer.is_empty() {
            let spec = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {"token": tokens.custom, "audiences": [AUDIENCE]}});
            match self.kube.call_as(reqwest::Method::POST, reviews, Some(&spec), &reviewer).await {
                Ok(r) if r.code == 403 => self.out.could_not_run("projected/token-review", t.elapsed(), format!("{REVIEWER} may not create tokenreviews (403): requires.toml service_account_cluster_rules")),
                Ok(r) => self.emit("projected/token-review", t, judge_review(&r, &self.ns)),
                Err(e) => self.out.failure("projected/token-review", t.elapsed(), &e, format!("TokenReview: {e:#}")),
            }
        }
        // The token is for that audience alone: the apiserver is not it.
        let t = Instant::now();
        match self.kube.call_as(reqwest::Method::GET, "/api", None, &tokens.custom).await {
            Ok(r) if r.code == 401 => self.emit("projected/other-audience-refused", t, pass("GET /api with the custom-audience token: 401")),
            Ok(r) => self.emit("projected/other-audience-refused", t, fail(format!("GET /api with the custom-audience token answered {}, want 401", r.code))),
            Err(e) => self.out.failure("projected/other-audience-refused", t.elapsed(), &e, format!("{e:#}")),
        }
    }

    /// Item 11: recorded, never a failure: pass where we have it, skip where not.
    async fn openshift(&self, tokens: Option<&Tokens>) {
        let have = |test: &str, t: Instant, got: std::result::Result<String, String>| match got {
            Ok(d) => self.emit(test, t, pass(d)),
            Err(d) => self.emit(test, t, (Status::Skip, format!("not here: {d}"))),
        };
        let t = Instant::now();
        let cms = format!("/api/v1/namespaces/{}/configmaps", self.ns);
        let got = match self.kube.get(&format!("{cms}/openshift-service-ca.crt")).await {
            Ok(r) if r.ok() => Ok("ConfigMap openshift-service-ca.crt is in the namespace".to_string()),
            Ok(r) => Err(format!("ConfigMap openshift-service-ca.crt answered {}", r.code)),
            Err(e) => Err(format!("{e:#}")),
        };
        have("openshift/service-ca-configmap", t, got);

        let t = Instant::now();
        let name = format!("pa-inject-{}", self.id);
        let body = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "labels": {"storm.io/test-run": self.run}, "annotations": {"service.beta.openshift.io/inject-cabundle": "true"}}});
        let got = match self.create(&cms, &body).await {
            Err(e) => Err(format!("{e:#}")),
            Ok(()) => {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let r = self.kube.get(&format!("{cms}/{name}")).await;
                    if r.as_ref().is_ok_and(|r| r.body["data"]["service-ca.crt"].is_string()) {
                        break Ok("an annotated ConfigMap was given service-ca.crt".to_string());
                    }
                    if Instant::now() >= deadline {
                        break Err("an annotated ConfigMap got no service-ca.crt in 10 s".to_string());
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        have("openshift/service-ca-injection", t, got);
        let _ = self.kube.delete(&format!("{cms}/{name}")).await;

        let t = Instant::now();
        let got = match self.kube.get(&format!("/api/v1/namespaces/{}/serviceaccounts/default", self.ns)).await {
            Ok(r) => match r.body["imagePullSecrets"].as_array().filter(|a| !a.is_empty()) {
                Some(a) => Ok(format!("the default ServiceAccount has {} imagePullSecret(s)", a.len())),
                None => Err("the default ServiceAccount has no imagePullSecrets".to_string()),
            },
            Err(e) => Err(format!("{e:#}")),
        };
        have("openshift/default-sa-pull-secret", t, got);

        let t = Instant::now();
        let got = match tokens {
            None => Err("no token from the pod to ask with".to_string()),
            Some(k) => match self.kube.call_as(reqwest::Method::GET, "/apis/user.openshift.io/v1/users/~", None, &k.default).await {
                Ok(r) if r.ok() => Ok(format!("users/~ names {}", r.body["metadata"]["name"])),
                Ok(r) => Err(format!("`oc whoami` asks GET /apis/user.openshift.io/v1/users/~: {}; `kubectl auth whoami` is authenticated/whoami", r.code)),
                Err(e) => Err(format!("{e:#}")),
            },
        };
        have("openshift/oc-whoami", t, got);
    }
}

fn running(p: &Value) -> bool {
    p["status"]["phase"] == "Running" && p["status"]["podIP"].as_str().is_some_and(|s| !s.is_empty())
}

fn ended(p: &Value) -> bool {
    matches!(p["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed"))
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Every item's line reading "could not run": the matrix keeps its shape.
fn none_ran(out: &Out, t: Instant, why: &str) {
    for test in ITEMS {
        out.could_not_run(test, t.elapsed(), why);
    }
}

pub async fn main(a: Args) -> i32 {
    let out = Out::new(&a.results, "podapi");
    let t0 = Instant::now();
    let kube = match Client::new(&a.api, a.token_file.as_deref(), a.insecure).await {
        Ok(k) => k,
        Err(e) => {
            out.could_not_run("api", t0.elapsed(), format!("{e:#}"));
            out.summary();
            return 2;
        }
    };
    let ns = a
        .namespace
        .clone()
        .unwrap_or_else(|| std::fs::read_to_string(format!("{SA}/namespace")).unwrap_or_else(|_| "default".into()).trim().to_string());
    let image = match a.image.clone() {
        Some(i) => Some(i),
        None => kube::own_image(&kube, &ns, &a.run_id).await,
    };
    let Some(image) = image else {
        none_ran(&out, t0, "this test's own image is unknown (not a runner pod and no --image)");
        out.summary();
        return out.exit_code();
    };
    let id: String = a.run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let d = Driver { kube: &kube, ns, run: a.run_id.clone(), id, image, pod_timeout: Duration::from_secs(a.pod_timeout), out: &out };
    if let Err(e) = checks(&d, &a, t0).await {
        none_ran(&out, t0, &format!("{e:#}"));
    }
    out.summary();
    out.exit_code()
}

/// The flavour the run is on (the matrix's other axis), from what runs here.
async fn flavour(d: &Driver<'_>) {
    let t = Instant::now();
    let cilium = d.kube.get("/apis/apps/v1/namespaces/kube-system/daemonsets/cilium").await;
    let detail = match cilium {
        Ok(r) if r.ok() => "cilium: a cilium DaemonSet runs in kube-system".to_string(),
        Ok(r) if r.code == 404 => "not cilium: no cilium DaemonSet in kube-system (flowsdn)".to_string(),
        Ok(r) => format!("unknown: reading the cilium DaemonSet answered {}", r.code),
        Err(e) => format!("unknown: {e:#}"),
    };
    d.out.emit(Line::new("flavour", Status::Pass, t.elapsed(), detail));
}

/// The probe up, then every item. `Err` before the probe reports: nothing ran.
async fn checks(d: &Driver<'_>, a: &Args, t0: Instant) -> Result<()> {
    flavour(d).await;
    let service = d.kube.get("/api/v1/namespaces/default/services/kubernetes").await?;
    anyhow::ensure!(service.ok(), "Service default/kubernetes answered {} (requires.toml cluster_read services)", service.code);
    let service_ip = service.body["spec"]["clusterIP"].as_str().unwrap_or("").to_string();
    let service_port = service.body["spec"]["ports"][0]["port"].as_u64().unwrap_or(443).to_string();

    // The ServiceAccounts: `default` comes from a controller; the reviewer's
    // rules are bound by the runner, to the name requires.toml gives.
    let sa = format!("/api/v1/namespaces/{}/serviceaccounts", d.ns);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.kube.get(&format!("{sa}/default")).await?.ok() {
        anyhow::ensure!(Instant::now() < deadline, "no default ServiceAccount in {} after 60 s", d.ns);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    d.create(&sa, &json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": REVIEWER, "labels": {"storm.io/test-run": d.run}}})).await?;

    let probe = format!("pa-probe-{}", d.id);
    d.create(&d.pods(), &d.probe_pod(&probe, &service_ip, &service_port)).await?;
    let pod = d.wait_pod(&probe, running).await?;
    anyhow::ensure!(running(&pod), "the probe pod is not Running in {} s: phase {}, {}", a.pod_timeout, pod["status"]["phase"], pod["status"]["message"]);
    let uid = pod["metadata"]["uid"].as_str().unwrap_or("").to_string();
    let pod_ip = pod["status"]["podIP"].as_str().unwrap_or("").to_string();
    let started = Instant::now();

    let (lines, ready) = d.wait_lines(&probe, d.pod_timeout, |l| find(l, "ready").is_some()).await;
    anyhow::ensure!(ready || !lines.is_empty(), "the probe pod reported nothing in {} s", a.pod_timeout);

    let t = Instant::now();
    d.emit("mounted/files", t, judge_mounted(&lines, &d.ns));
    d.emit("env/set", t, judge_env(&lines));
    d.emit("env/service-ip", t, judge_env_service(&lines, &service_ip, &service_port));
    d.emit("reachable/dns", t, judge_dns(&lines, &service_ip));
    d.emit("reachable/by-name", t, judge_https(&lines, "name"));
    d.emit("reachable/by-service-ip", t, judge_https(&lines, "service-ip"));
    d.emit("authenticated/api", t, judge_api(&lines));
    d.emit("authenticated/whoami", t, judge_whoami(&lines, &d.ns, "default"));
    let projected = has_token_projection(&pod);
    d.emit("bound/projected-token", t, if projected { judge_default_token(&lines, &probe, &uid, now_secs()) } else { fail("the pod has no projected serviceAccountToken volume at the ServiceAccount path") });
    d.emit("projected/custom-token", t, judge_custom_token(&lines, &probe, &uid));
    d.emit("downward/env", t, judge_downward_env(&lines, &pod));
    d.emit("downward/volume", t, judge_downward_volume(&lines, &d.id));

    let t = Instant::now();
    let tokens = match fetch_tokens(&format!("{pod_ip}:{SERVE_PORT}")).await {
        Ok(k) if !k.default.is_empty() && !k.custom.is_empty() => Some(k),
        Ok(_) => {
            for test in ["projected/token-review", "projected/other-audience-refused"] {
                d.out.emit(Line::new(test, Status::Fail, t.elapsed(), "the pod has no token to give"));
            }
            None
        }
        Err(e) => {
            for test in ["projected/token-review", "projected/other-audience-refused"] {
                d.out.could_not_run(test, t.elapsed(), format!("the tokens from the pod: {e:#}"));
            }
            None
        }
    };
    if let Some(k) = &tokens {
        d.review(k).await;
    }

    let within = Duration::from_secs(a.rbac_within);
    d.rbac(&probe, within).await;
    d.watch(&probe, within, Duration::from_secs(a.watch_hold)).await;
    d.optout().await;
    d.openshift(tokens.as_ref()).await;

    // The rotation: what is left of its wait, inside the budget.
    let t = Instant::now();
    let left_in_run = Duration::from_secs(a.timeout.saturating_sub(120)).saturating_sub(t0.elapsed());
    let wait = Duration::from_secs(a.rotation_timeout).saturating_sub(started.elapsed()).min(left_in_run);
    let (lines, _) = d
        .wait_lines(&probe, wait, |l| {
            let jtis: HashSet<&str> = token_claims(l, "custom").iter().filter_map(|c| c["claims"]["jti"].as_str()).collect();
            jtis.len() > 1
        })
        .await;
    d.emit("bound/rotated", t, judge_rotated(&lines));
    d.out.emit(Line::new(
        "bound/rotated-default",
        Status::Skip,
        Duration::ZERO,
        format!("the default token rewrites at 80% of {DEFAULT_TTL} s (48 min), past a day run's budget; bound/rotated is the same path on a {CUSTOM_TTL} s token"),
    ));

    // A bound token dies with its pod.
    let t = Instant::now();
    match &tokens {
        None => d.out.could_not_run("bound/refused-after-delete", t.elapsed(), "no token from the pod to try"),
        Some(k) => match after_delete(d, &probe, &k.default, within).await {
            Ok(v) => d.emit("bound/refused-after-delete", t, v),
            Err(e) => d.out.failure("bound/refused-after-delete", t.elapsed(), &e, format!("{e:#}")),
        },
    }
    Ok(())
}

/// Item 6: the pod's token works, the pod goes, the token is refused.
async fn after_delete(d: &Driver<'_>, pod: &str, token: &str, within: Duration) -> Result<Verdict> {
    let review = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "SelfSubjectReview"});
    let path = "/apis/authentication.k8s.io/v1/selfsubjectreviews";
    let before = d.kube.call_as(reqwest::Method::POST, path, Some(&review), token).await?;
    if !before.ok() {
        return Ok(fail(format!("the token answered {} while its pod was there", before.code)));
    }
    d.kube.delete(&format!("{}/{pod}", d.pods())).await?;
    let gone = Instant::now() + d.pod_timeout;
    while d.pod_state(pod).await?.code != 404 {
        if Instant::now() >= gone {
            return Ok(fail(format!("the pod is still there {} s after its delete", d.pod_timeout.as_secs())));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let t = Instant::now();
    loop {
        let r = d.kube.call_as(reqwest::Method::POST, path, Some(&review), token).await?;
        if r.code == 401 {
            return Ok(pass(format!("{} while the pod was there, 401 {} s after it was gone", before.code, t.elapsed().as_secs())));
        }
        if t.elapsed() >= within {
            return Ok(fail(format!("still {} {} s after its pod was gone", r.code, within.as_secs())));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ---- the probe: `/test podapi-probe` ----

#[derive(Parser, Debug)]
#[command(name = "test podapi-probe", about = "What this pod can do with the API, one JSON line per fact (#82)")]
pub struct ProbeArgs {
    /// Report only the mount, and end (item 7's pods).
    #[arg(long)]
    opt_out: bool,
}

/// A base64url (no padding) decoder: a token's claims, and nothing else here.
pub fn b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// A token's claims, for the log: named fields of the payload. Never the
/// signature, so what is printed is not a credential.
pub fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let c: Value = serde_json::from_slice(&b64url(payload)?).ok()?;
    let kube = &c["kubernetes.io"];
    let aud = match &c["aud"] {
        Value::String(s) => json!([s]),
        other => other.clone(),
    };
    Some(json!({"aud": aud, "iat": c["iat"], "nbf": c["nbf"], "exp": c["exp"], "jti": c["jti"], "iss": c["iss"], "sub": c["sub"],
                "warnafter": kube["warnafter"], "pod": {"name": kube["pod"]["name"], "uid": kube["pod"]["uid"]}}))
}

fn read_token(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn default_token() -> String {
    read_token(&format!("{SA}/token"))
}

fn custom_token() -> String {
    read_token(&format!("{CUSTOM_DIR}/token"))
}

fn env(name: &str) -> Value {
    std::env::var(name).map_or(Value::Null, Value::String)
}

/// A client trusting the mounted CA alone. `whole` bounds a request; a watch
/// has none.
fn pod_client(ca: &[u8], whole: Option<Duration>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().tls_built_in_root_certs(false).connect_timeout(Duration::from_secs(5));
    if let Some(w) = whole {
        b = b.timeout(w);
    }
    for c in reqwest::Certificate::from_pem_bundle(ca).context("ca.crt is not PEM")? {
        b = b.add_root_certificate(c);
    }
    Ok(b.build()?)
}

fn chain(e: reqwest::Error) -> String {
    format!("{:#}", anyhow::Error::from(e))
}

/// Item 3's look: an answer, or why not; a non-answer is tried again (#57).
async fn https(client: &reqwest::Client, via: &str, url: &str) {
    let token = default_token();
    let (r, _) = retry::probe(&retry::Policy::PROBE, url, |_| async {
        let r = client.get(url).bearer_auth(&token).send().await;
        let ok = r.is_ok();
        (r, ok)
    })
    .await;
    match r {
        Ok(a) => say(json!({"pa": "https", "via": via, "url": url, "code": a.status().as_u16()})),
        Err(e) => say(json!({"pa": "https", "via": via, "url": url, "err": chain(e)})),
    }
}

/// The base every later call goes through: the Service's name, as a client's
/// would; when that does not resolve, the environment's, so a DNS failure does
/// not also read as "not authenticated".
fn base_of(resolved: bool, host: &str, port: &str) -> String {
    if resolved { format!("https://{API_NAME}") } else { format!("https://{host}:{port}") }
}

/// Item 4, and again after a rotation.
async fn whoami(client: &reqwest::Client, base: &str) {
    let body = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "SelfSubjectReview"});
    let url = format!("{base}/apis/authentication.k8s.io/v1/selfsubjectreviews");
    match client.post(url).bearer_auth(default_token()).json(&body).send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            let info = &v["status"]["userInfo"];
            say(json!({"pa": "whoami", "code": code, "username": info["username"], "groups": info["groups"]}));
        }
        Err(e) => say(json!({"pa": "whoami", "err": chain(e)})),
    }
}

/// Item 5: one line whenever the answers change. Each look is its own: a failed
/// one (code 0) is the next look's to correct.
async fn poll_rbac(client: reqwest::Client, base: String, ns: String, pod: String) {
    let mut last = None;
    loop {
        let token = default_token();
        let look = |url: String| {
            let (client, token) = (client.clone(), token.clone());
            async move { client.get(url).bearer_auth(token).send().await.map_or(0, |r| r.status().as_u16()) }
        };
        let get_pods = look(format!("{base}/api/v1/namespaces/{ns}/pods/{pod}")).await;
        let list_secrets = look(format!("{base}/api/v1/namespaces/{ns}/secrets?limit=1")).await;
        if last != Some((get_pods, list_secrets)) {
            say(json!({"pa": "rbac", "get_pods": get_pods, "list_secrets": list_secrets}));
            last = Some((get_pods, list_secrets));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Item 6 and 8: one line per token seen, claims only.
async fn poll_tokens(client: reqwest::Client, base: String) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut first = true;
    loop {
        for (vol, token) in [("default", default_token()), ("custom", custom_token())] {
            if let Some(c) = claims(&token)
                && seen.insert(c["jti"].as_str().unwrap_or("").to_string())
            {
                say(json!({"pa": "token", "vol": vol, "claims": c}));
                if !first && vol == "default" {
                    whoami(&client, &base).await;
                }
            }
        }
        first = false;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Item 10: the watch, once it may. One line per open or end, and per ADDED or
/// DELETED: a long-lived connection through the Service.
async fn hold_watch(client: reqwest::Client, base: String, ns: String) {
    let url = format!("{base}/api/v1/namespaces/{ns}/pods?watch=true&timeoutSeconds=1800");
    let mut last_refusal = None;
    loop {
        let r = match client.get(&url).bearer_auth(default_token()).send().await {
            Ok(r) => r,
            Err(e) => {
                let why = chain(e);
                if last_refusal.as_ref() != Some(&why) {
                    say(json!({"pa": "watch", "state": "error", "why": why}));
                    last_refusal = Some(why);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if !r.status().is_success() {
            let why = r.status().as_u16().to_string();
            if last_refusal.as_ref() != Some(&why) {
                say(json!({"pa": "watch", "state": "refused", "why": why}));
                last_refusal = Some(why);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        say(json!({"pa": "watch", "state": "open", "base": base}));
        let mut r = r;
        let mut buf: Vec<u8> = Vec::new();
        let why = loop {
            match r.chunk().await {
                Ok(Some(b)) => buf.extend_from_slice(&b),
                Ok(None) => break "stream ended".to_string(),
                Err(e) => break chain(e),
            }
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                if let Ok(v) = serde_json::from_slice::<Value>(&line)
                    && matches!(v["type"].as_str(), Some("ADDED" | "DELETED"))
                {
                    say(json!({"pa": "event", "type": v["type"], "name": v["object"]["metadata"]["name"]}));
                }
            }
        };
        say(json!({"pa": "watch", "state": "ended", "why": why}));
    }
}

/// Gives this pod's tokens to whoever connects: the driver, for a TokenReview
/// and for trying one after the pod is gone. Over the pod network and not the
/// log: a log is kept, and no log may hold a token. The run's namespace holds
/// only this run's pods, and the `default` ServiceAccount may do nothing.
async fn give_tokens(listener: tokio::net::TcpListener) {
    while let Ok((mut c, _)) = listener.accept().await {
        let text = json!({"default": default_token(), "custom": custom_token()}).to_string();
        let _ = c.write_all(text.as_bytes()).await;
    }
}

pub async fn probe(a: ProbeArgs) -> i32 {
    let token_there = !default_token().is_empty();
    let ca = std::fs::read(format!("{SA}/ca.crt"));
    let ns_file = std::fs::read_to_string(format!("{SA}/namespace")).ok().map(|s| s.trim().to_string());
    say(json!({"pa": "mounted", "token": token_there, "ca": ca.is_ok(), "namespace": ns_file}));
    if a.opt_out {
        return 0;
    }

    let (host, port) = (std::env::var("KUBERNETES_SERVICE_HOST").unwrap_or_default(), std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_default());
    say(json!({"pa": "env", "KUBERNETES_SERVICE_HOST": env("KUBERNETES_SERVICE_HOST"), "KUBERNETES_SERVICE_PORT": env("KUBERNETES_SERVICE_PORT"), "KUBERNETES_SERVICE_PORT_HTTPS": env("KUBERNETES_SERVICE_PORT_HTTPS")}));
    say(json!({"pa": "downward-env", "POD_NAME": env("POD_NAME"), "POD_NAMESPACE": env("POD_NAMESPACE"), "POD_IP": env("POD_IP"), "NODE_NAME": env("NODE_NAME"), "LIMIT_MEMORY": env("LIMIT_MEMORY"), "LIMIT_CPU": env("LIMIT_CPU")}));
    let mut vol = json!({"pa": "downward-volume"});
    for file in ["labels", "annotations"] {
        match std::fs::read_to_string(format!("{INFO_DIR}/{file}")) {
            Ok(text) => vol[file] = json!(text),
            Err(e) => vol[format!("{file}_err")] = json!(e.to_string()),
        }
    }
    say(vol);

    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let nameservers: Vec<&str> = resolv.lines().filter_map(|l| l.strip_prefix("nameserver")).map(str::trim).collect();
    let (looked, _) = retry::probe(&retry::Policy::PROBE, &format!("dns {API_NAME}"), |_| async {
        let r = tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((API_NAME, 443))).await.map(|r| r.map(|it| it.map(|s| s.ip().to_string()).collect::<Vec<_>>()));
        let ok = matches!(&r, Ok(Ok(v)) if !v.is_empty());
        (r, ok)
    })
    .await;
    let resolved = match looked {
        Ok(Ok(addrs)) if !addrs.is_empty() => {
            say(json!({"pa": "dns", "ok": true, "addrs": addrs}));
            true
        }
        Ok(Ok(_)) => {
            say(json!({"pa": "dns", "ok": false, "why": "no addresses", "nameservers": nameservers}));
            false
        }
        Ok(Err(e)) => {
            say(json!({"pa": "dns", "ok": false, "why": e.to_string(), "nameservers": nameservers}));
            false
        }
        Err(_) => {
            say(json!({"pa": "dns", "ok": false, "why": "no answer in 10 s", "nameservers": nameservers}));
            false
        }
    };

    // The tokens' server first: the driver may ask from `ready` on.
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", SERVE_PORT)).await {
        Ok(l) => Some(l),
        Err(e) => {
            say(json!({"pa": "listen", "err": e.to_string()}));
            None
        }
    };
    if let Some(l) = listener {
        tokio::spawn(give_tokens(l));
    }

    let client = ca.as_ref().ok().and_then(|ca| pod_client(ca, Some(Duration::from_secs(10))).ok());
    let watcher = ca.as_ref().ok().and_then(|ca| pod_client(ca, None).ok());
    let base = base_of(resolved, &host, &port);
    let ns = std::env::var("POD_NAMESPACE").ok().or(ns_file).unwrap_or_default();
    let pod = std::env::var("POD_NAME").unwrap_or_default();
    let (service_ip, service_port) = (std::env::var("PA_SERVICE_IP").unwrap_or_default(), std::env::var("PA_SERVICE_PORT").unwrap_or_default());
    match (client, watcher) {
        (Some(client), Some(watcher)) => {
            https(&client, "name", &format!("https://{API_NAME}/version")).await;
            https(&client, "service-ip", &format!("https://{service_ip}:{service_port}/version")).await;
            let (r, _) = retry::probe(&retry::Policy::PROBE, "GET /api", |_| async {
                let r = client.get(format!("{base}/api")).bearer_auth(default_token()).send().await;
                let ok = r.is_ok();
                (r, ok)
            })
            .await;
            match r {
                Ok(a) => {
                    let code = a.status().as_u16();
                    let v: Value = a.json().await.unwrap_or(Value::Null);
                    say(json!({"pa": "api", "base": base, "code": code, "kind": v["kind"]}));
                }
                Err(e) => say(json!({"pa": "api", "base": base, "err": chain(e)})),
            }
            whoami(&client, &base).await;
            tokio::spawn(poll_tokens(client.clone(), base.clone()));
            tokio::spawn(poll_rbac(client, base.clone(), ns.clone(), pod));
            tokio::spawn(hold_watch(watcher, base, ns));
        }
        _ => {
            for pa in ["api", "whoami"] {
                say(json!({"pa": pa, "err": format!("no client: {SA}/ca.crt is missing or not PEM")}));
            }
            for via in ["name", "service-ip"] {
                say(json!({"pa": "https", "via": via, "url": API_NAME, "err": format!("no client: {SA}/ca.crt is missing or not PEM")}));
            }
        }
    }
    // Let the token poller's first look land before the driver reads.
    tokio::time::sleep(Duration::from_secs(1)).await;
    say(json!({"pa": "ready"}));
    tokio::time::sleep(PROBE_LIFE).await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // allow-secret: a fake JWT fixture, signature "sig".
    fn fake_jwt(payload: &str) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for c in payload.as_bytes().chunks(3) {
            let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
            for (i, shift) in [18, 12, 6, 0].iter().enumerate() {
                if i <= c.len() {
                    s.push(T[((n >> shift) & 63) as usize] as char);
                }
            }
        }
        format!("eyJh.{s}.sig")
    }

    fn pod_claims(iat: i64, exp: i64, warn: Option<i64>, aud: Value) -> Value {
        json!({"pa": "token", "vol": "x", "claims": {"aud": aud, "iat": iat, "exp": exp, "warnafter": warn, "jti": "a", "pod": {"name": "p", "uid": "u"}}})
    }

    fn with_vol(mut v: Value, vol: &str) -> Value {
        v["vol"] = json!(vol);
        v
    }

    #[test]
    fn log_lines_survive_a_runtime_warning_and_the_unspacing() {
        let log = "warning: /proc could not be mounted\n{\"pa\":\"dns\",\"why\":\"no·answer\"}\n{\"other\":1}\n";
        let l = parse(log);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["why"], "no answer");
    }

    #[test]
    fn claims_are_read_without_the_signature() {
        let t = fake_jwt(r#"{"aud":"one","iat":10,"exp":20,"jti":"j","sub":"s","kubernetes.io":{"warnafter":15,"pod":{"name":"p","uid":"u"}}}"#);
        let c = claims(&t).unwrap();
        assert_eq!(c["aud"], json!(["one"]), "a single aud reads as a list");
        assert_eq!((c["iat"].as_i64(), c["warnafter"].as_i64()), (Some(10), Some(15)));
        assert_eq!(c["pod"]["name"], "p");
        assert!(!c.to_string().contains("sig"), "what is logged is not a credential");
        assert!(claims("not-a-jwt").is_none());
        assert_eq!(b64url("eyJzdWIiOiJ4In0").unwrap(), br#"{"sub":"x"}"#);
        assert!(b64url("a b").is_none());
    }

    #[test]
    fn mounted_names_each_gap_and_the_wrong_namespace() {
        let good = [json!({"pa": "mounted", "token": true, "ca": true, "namespace": "ns"})];
        assert_eq!(judge_mounted(&good, "ns").0, Status::Pass);
        assert!(judge_mounted(&good, "other").1.contains("the pod is in"));
        let none = [json!({"pa": "mounted", "token": false, "ca": false, "namespace": null})];
        let (st, d) = judge_mounted(&none, "ns");
        assert_eq!(st, Status::Fail);
        assert!(d.contains("no token") && d.contains("no ca.crt") && d.contains("no namespace"));
        assert!(judge_mounted(&[], "ns").1.contains("reported nothing"));
    }

    #[test]
    fn env_is_set_apart_from_naming_the_service() {
        let e = [json!({"pa": "env", "KUBERNETES_SERVICE_HOST": "192.168.31.172", "KUBERNETES_SERVICE_PORT": "6443", "KUBERNETES_SERVICE_PORT_HTTPS": "6443"})];
        assert_eq!(judge_env(&e).0, Status::Pass);
        let (st, d) = judge_env_service(&e, "10.96.0.1", "443");
        assert_eq!(st, Status::Fail, "the node's apiserver is not the Service");
        assert!(d.contains("10.96.0.1:443"));
        let s = [json!({"pa": "env", "KUBERNETES_SERVICE_HOST": "10.96.0.1", "KUBERNETES_SERVICE_PORT": "443", "KUBERNETES_SERVICE_PORT_HTTPS": "443"})];
        assert_eq!(judge_env_service(&s, "10.96.0.1", "443").0, Status::Pass);
        let unset = [json!({"pa": "env", "KUBERNETES_SERVICE_HOST": "h", "KUBERNETES_SERVICE_PORT": null})];
        assert!(judge_env(&unset).1.contains("KUBERNETES_SERVICE_PORT_HTTPS"));
    }

    #[test]
    fn dns_and_tls_failures_read_differently() {
        let ok = [json!({"pa": "dns", "ok": true, "addrs": ["10.96.0.1"]})];
        assert_eq!(judge_dns(&ok, "10.96.0.1").0, Status::Pass);
        assert!(judge_dns(&ok, "10.96.0.9").1.contains("the kubernetes Service is"));
        let no = [json!({"pa": "dns", "ok": false, "why": "no answer", "nameservers": ["10.96.0.10"]})];
        assert!(judge_dns(&no, "10.96.0.1").1.contains("did not resolve"));
        let tls = [json!({"pa": "https", "via": "name", "url": "u", "err": "invalid peer certificate"}), json!({"pa": "https", "via": "service-ip", "url": "u", "code": 403})];
        assert!(judge_https(&tls, "name").1.contains("invalid peer certificate"));
        assert_eq!(judge_https(&tls, "service-ip").0, Status::Pass, "an HTTP answer is reached, whatever it allows");
        let down = [json!({"pa": "https", "via": "name", "url": "u", "code": 503})];
        assert_eq!(judge_https(&down, "name").0, Status::Fail);
    }

    #[test]
    fn whoami_wants_the_service_account_and_its_groups() {
        let groups = json!(["system:serviceaccounts", "system:serviceaccounts:ns", "system:authenticated"]);
        let good = [json!({"pa": "whoami", "code": 201, "username": "system:serviceaccount:ns:default", "groups": groups})];
        assert_eq!(judge_whoami(&good, "ns", "default").0, Status::Pass);
        assert!(judge_whoami(&good, "other", "default").1.contains("want"));
        let short = [json!({"pa": "whoami", "code": 201, "username": "system:serviceaccount:ns:default", "groups": ["system:authenticated"]})];
        assert!(judge_whoami(&short, "ns", "default").1.contains("system:serviceaccounts:ns"));
        let refused = [json!({"pa": "whoami", "code": 403})];
        assert_eq!(judge_whoami(&refused, "ns", "default").0, Status::Fail);
    }

    #[test]
    fn the_default_token_lasts_an_hour_even_when_extended() {
        let now = 1000;
        let v = |c| vec![with_vol(c, "default")];
        // Extended as upstream: a year in exp, the hour in warnafter.
        let ext = v(pod_claims(900, 900 + 365 * 86_400, Some(900 + 3607), json!(["api"])));
        let (st, d) = judge_default_token(&ext, "p", "u", now);
        assert_eq!(st, Status::Pass, "{d}");
        assert!(d.contains("warnafter"));
        // Not extended: exp is the hour.
        assert_eq!(judge_default_token(&v(pod_claims(900, 900 + 3607, None, json!(["api"]))), "p", "u", now).0, Status::Pass);
        // A long token with no warnafter is not bound to an hour.
        assert!(judge_default_token(&v(pod_claims(900, 900 + 86_400, None, json!(["api"]))), "p", "u", now).1.contains("lasts"));
        // Expired, no audience, another pod.
        assert!(judge_default_token(&v(pod_claims(0, 500, None, json!(["api"]))), "p", "u", now).1.contains("expired"));
        assert!(judge_default_token(&v(pod_claims(900, 900 + 3607, None, json!([]))), "p", "u", now).1.contains("audience"));
        assert!(judge_default_token(&v(pod_claims(900, 900 + 3607, None, json!(["api"]))), "p", "other", now).1.contains("bound to"));
    }

    #[test]
    fn the_token_mount_must_be_a_projection() {
        let pod = |source: Value| {
            json!({"spec": {"containers": [{"volumeMounts": [{"name": "kube-api-access-x", "mountPath": SA}]}],
                            "volumes": [{"name": "kube-api-access-x", "projected": {"sources": [source]}}]}})
        };
        assert!(has_token_projection(&pod(json!({"serviceAccountToken": {"path": "token"}}))));
        assert!(!has_token_projection(&pod(json!({"configMap": {"name": "x"}}))));
        assert!(!has_token_projection(&json!({"spec": {"containers": [{}]}})), "no mount: the kubelet's fallback");
    }

    #[test]
    fn the_custom_token_has_what_it_asked_for() {
        let v = |c| vec![with_vol(c, "custom")];
        assert_eq!(judge_custom_token(&v(pod_claims(0, 600, None, json!([AUDIENCE]))), "p", "u").0, Status::Pass);
        assert!(judge_custom_token(&v(pod_claims(0, 600, None, json!(["api"]))), "p", "u").1.contains("audience"));
        assert!(judge_custom_token(&v(pod_claims(0, 3600, None, json!([AUDIENCE]))), "p", "u").1.contains("lasts 3600"));
    }

    #[test]
    fn rotation_must_land_before_the_first_token_ends() {
        let t = |iat, exp, jti| json!({"pa": "token", "vol": "custom", "claims": {"iat": iat, "exp": exp, "jti": jti}});
        assert!(judge_rotated(&[t(0, 600, "a")]).1.contains("not rewritten"));
        let (st, d) = judge_rotated(&[t(0, 600, "a"), t(480, 1080, "b")]);
        assert_eq!(st, Status::Pass);
        assert!(d.contains("480 s into its 600 s"));
        assert_eq!(judge_rotated(&[t(0, 600, "a"), t(700, 1300, "b")]).0, Status::Fail);
    }

    #[test]
    fn downward_env_compares_with_the_api_and_names_what_is_unset() {
        let pod = json!({"metadata": {"name": "p", "namespace": "ns"}, "status": {"podIP": "10.0.0.5"}, "spec": {"nodeName": "n1"}});
        let good = [json!({"pa": "downward-env", "POD_NAME": "p", "POD_NAMESPACE": "ns", "POD_IP": "10.0.0.5", "NODE_NAME": "n1", "LIMIT_MEMORY": LIMIT_MEMORY_BYTES, "LIMIT_CPU": LIMIT_CPU_CORES})];
        assert_eq!(judge_downward_env(&good, &pod).0, Status::Pass);
        // resourceFieldRef not resolved, an empty pod IP.
        let bad = [json!({"pa": "downward-env", "POD_NAME": "p", "POD_NAMESPACE": "ns", "POD_IP": "", "NODE_NAME": "n1", "LIMIT_MEMORY": null, "LIMIT_CPU": null})];
        let (st, d) = judge_downward_env(&bad, &pod);
        assert_eq!(st, Status::Fail);
        assert!(d.contains("LIMIT_MEMORY") && d.contains("LIMIT_CPU") && d.contains("POD_IP"));
    }

    #[test]
    fn the_downward_volume_holds_this_pods_labels_and_annotations() {
        let good = [json!({"pa": "downward-volume", "labels": "a=\"b\"\nstorm.io/podapi=\"i1\"\n", "annotations": "storm.io/podapi-note=\"i1\""})];
        assert_eq!(judge_downward_volume(&good, "i1").0, Status::Pass);
        let empty = [json!({"pa": "downward-volume", "labels": "", "annotations_err": "No such file"})];
        let (st, d) = judge_downward_volume(&empty, "i1");
        assert_eq!(st, Status::Fail);
        assert!(d.contains("No such file") && d.contains("storm.io/podapi="));
    }

    #[test]
    fn opting_out_means_no_token() {
        assert_eq!(judge_optout(&[json!({"pa": "mounted", "token": false})], "Succeeded").0, Status::Pass);
        assert_eq!(judge_optout(&[json!({"pa": "mounted", "token": true})], "Succeeded").0, Status::Fail);
        assert!(judge_optout(&[], "Failed").1.contains("Failed"));
    }

    #[test]
    fn the_watch_is_one_connection_with_both_events() {
        let open = json!({"pa": "watch", "state": "open", "base": "https://kubernetes.default.svc"});
        let ev = |ty: &str| json!({"pa": "event", "type": ty, "name": "m"});
        let hold = Duration::from_secs(60);
        assert_eq!(judge_watch(&[open.clone(), ev("ADDED"), ev("DELETED")], "m", hold).0, Status::Pass);
        assert!(judge_watch(&[open.clone(), ev("ADDED")], "m", hold).1.contains("no DELETED"));
        let ended = json!({"pa": "watch", "state": "ended", "why": "stream ended"});
        assert!(judge_watch(&[open.clone(), ev("ADDED"), ended, open, ev("DELETED")], "m", hold).1.contains("did not last"));
        let refused = json!({"pa": "watch", "state": "refused", "why": "403"});
        assert!(judge_watch(&[refused], "m", hold).1.contains("never opened"));
    }

    #[test]
    fn the_review_wants_the_audience_and_the_user_and_never_prints_the_echo() {
        let resp = |body| kube::Resp { code: 201, body };
        let good = json!({"spec": {"token": "SECRET"}, "status": {"authenticated": true, "audiences": [AUDIENCE], "user": {"username": "system:serviceaccount:ns:default"}}});
        let (st, d) = judge_review(&resp(good), "ns");
        assert_eq!(st, Status::Pass);
        assert!(!d.contains("SECRET"));
        let no = json!({"spec": {"token": "SECRET"}, "status": {"authenticated": false, "error": "bad audience"}});
        let (st, d) = judge_review(&resp(no), "ns");
        assert_eq!(st, Status::Fail);
        assert!(d.contains("bad audience") && !d.contains("SECRET"));
        let other = json!({"status": {"authenticated": true, "audiences": ["api"], "user": {"username": "x"}}});
        assert!(judge_review(&resp(other), "ns").1.contains("lack"));
        assert!(judge_review(&kube::Resp { code: 403, body: Value::Null }, "ns").1.contains("403"));
    }

    #[tokio::test]
    async fn tokens_come_over_tcp_and_an_error_names_no_content() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            for answer in [&br#"{"default":"d","custom":"c"}"#[..], b"SECRET not json"] {
                // Dropped each turn: the end of the answer is the connection's end.
                let (mut c, _) = l.accept().await.unwrap();
                c.write_all(answer).await.unwrap();
            }
        });
        let k = fetch_tokens(&addr).await.unwrap();
        assert_eq!((k.default.as_str(), k.custom.as_str()), ("d", "c"));
        let e = format!("{:#}", fetch_tokens(&addr).await.err().unwrap());
        assert!(!e.contains("SECRET"), "{e}");
    }

    #[test]
    fn the_base_falls_back_to_the_environment_when_dns_fails() {
        assert_eq!(base_of(true, "h", "1"), format!("https://{API_NAME}"));
        assert_eq!(base_of(false, "h", "1"), "https://h:1");
    }
}
