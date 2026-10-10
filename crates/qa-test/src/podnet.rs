//! The ordinary pod network (#35, from stormcos#51: "the pod network comes
//! up and a pod can resolve and reach a Service"), in the run namespace.
//!
//! A `/test serve` pod behind a ClusterIP Service; a second pod,
//! `/test resolve`, looks up `<svc>.<ns>.svc.cluster.local` through the
//! cluster resolver, connects to what it got, and connects to the ClusterIP
//! directly, so a DNS failure and a Service-routing failure read
//! differently:
//!
//! - `pod-network/dns`: the name resolves (and to the ClusterIP);
//! - `pod-network/service-by-name`: the resolved address answers;
//! - `pod-network/service-by-ip`: the ClusterIP answers.
//!
//! The resolve pod prints one JSON line per step with no spaces in it, so
//! rustkube-node#136 cannot cut it.

use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::agent::SERVE_PORT;
use crate::kube::Client;
use crate::report::{Line, Out, Status};

/// Run the case; returns (failed, could not run) counts it emitted.
pub async fn run(kube: &Client, ns: &str, run_id: &str, image: &str, out: &Out) -> (usize, usize) {
    let t = Instant::now();
    let id: String = run_id.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let (serve, svc, client) = (format!("pn-serve-{id}"), format!("pn-{id}"), format!("pn-resolve-{id}"));
    let pods = format!("/api/v1/namespaces/{ns}/pods");
    let services = format!("/api/v1/namespaces/{ns}/services");
    let labels = json!({"storm.io/test-run": run_id, "storm.io/pod-network": id});
    let pod = |name: &str, args: Vec<&str>, env: Value| {
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": name, "labels": if args[0] == "serve" { labels.clone() } else { json!({"storm.io/test-run": run_id}) }},
               "spec": {"restartPolicy": "Never", "terminationGracePeriodSeconds": 1, "automountServiceAccountToken": false,
                        "containers": [{"name": "c", "image": image, "command": ["/test"], "args": args, "env": env}]}})
    };
    let result = async {
        for (path, body) in [
            (&pods, pod(&serve, vec!["serve"], json!([]))),
            (&services, json!({"apiVersion": "v1", "kind": "Service",
                "metadata": {"name": svc, "labels": {"storm.io/test-run": run_id}},
                "spec": {"selector": {"storm.io/pod-network": id}, "ports": [{"port": SERVE_PORT, "targetPort": SERVE_PORT, "protocol": "TCP"}]}})),
        ] {
            let r = kube.post(path, &body).await.map_err(|e| format!("{e:#}"))?;
            if !r.ok() {
                return Err(format!("creating {} answered {}: {}", body["metadata"]["name"], r.code, r.body));
            }
        }
        // The Service's ClusterIP, and an endpoint behind it.
        let deadline = Instant::now() + Duration::from_secs(120);
        let cluster_ip = loop {
            let s = kube.get(&format!("{services}/{svc}")).await.map_err(|e| format!("{e:#}"))?;
            let ep = kube.get(&format!("/api/v1/namespaces/{ns}/endpoints/{svc}")).await.map_err(|e| format!("{e:#}"))?;
            let ip = s.body["spec"]["clusterIP"].as_str().unwrap_or("").to_string();
            let ready = ep.body["subsets"].as_array().is_some_and(|ss| ss.iter().any(|s| s["addresses"].as_array().is_some_and(|a| !a.is_empty())));
            if !ip.is_empty() && ip != "None" && ready {
                break ip;
            }
            if Instant::now() >= deadline {
                let phase = kube.get(&format!("{pods}/{serve}")).await.ok().and_then(|r| r.body["status"]["phase"].as_str().map(str::to_string));
                return Err(format!("Service {svc} not ready in 120 s: clusterIP {ip:?}, endpoint ready {ready}, serve pod {phase:?}"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let name = format!("{svc}.{ns}.svc.cluster.local");
        let env = json!([{"name": "PN_NAME", "value": name}, {"name": "PN_IP", "value": cluster_ip}, {"name": "PN_PORT", "value": SERVE_PORT.to_string()}]);
        let r = kube.post(&pods, &pod(&client, vec!["resolve"], env)).await.map_err(|e| format!("{e:#}"))?;
        if !r.ok() {
            return Err(format!("creating {client} answered {}: {}", r.code, r.body));
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        let phase = loop {
            let p = kube.get(&format!("{pods}/{client}")).await.map_err(|e| format!("{e:#}"))?;
            let phase = p.body["status"]["phase"].as_str().unwrap_or("").to_string();
            if phase == "Succeeded" || phase == "Failed" || Instant::now() >= deadline {
                break phase;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let (_, log) = kube.text(&format!("{pods}/{client}/log")).await.map_err(|e| format!("{e:#}"))?;
        Ok((name, cluster_ip, phase, log))
    }
    .await;

    for path in [format!("{pods}/{client}"), format!("{pods}/{serve}"), format!("{services}/{svc}")] {
        let _ = kube.delete(&path).await;
    }
    match result {
        Err(why) => {
            out.emit(Line::new("pod-network", Status::Fail, t.elapsed(), why));
            (1, 0)
        }
        Ok((name, ip, phase, log)) => {
            let mut failed = 0;
            for (test, step) in [("pod-network/dns", "dns"), ("pod-network/service-by-name", "by-name"), ("pod-network/service-by-ip", "by-ip")] {
                let (st, d) = verdict(&log, step, &name, &ip, &phase);
                if st == Status::Fail {
                    failed += 1;
                }
                out.emit(Line::new(test, st, t.elapsed(), d));
            }
            (failed, 0)
        }
    }
}

/// One step's line from the resolve pod's log.
pub fn verdict(log: &str, step: &str, name: &str, ip: &str, phase: &str) -> (Status, String) {
    let found = log.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).find(|v| v["pn"] == step);
    match found {
        None => (Status::Fail, format!("the resolve pod reported nothing for {step} (phase {phase:?})")),
        Some(v) => {
            let detail = v["detail"].as_str().unwrap_or("").replace('·', " ");
            match (step, v["ok"] == true) {
                ("dns", true) if !detail.split(',').any(|a| a == ip) => (Status::Fail, format!("{name} resolved to {detail}, not the ClusterIP {ip}")),
                ("dns", true) => (Status::Pass, format!("{name} -> {detail}")),
                (_, true) => (Status::Pass, detail),
                (_, false) => (Status::Fail, detail),
            }
        }
    }
}

// ---- the resolve pod: `/test resolve` ----

#[derive(Parser, Debug)]
#[command(name = "test resolve", about = "Resolve a Service by name and reach it, and by ClusterIP")]
pub struct ResolveArgs {
    #[arg(long, env = "PN_NAME")]
    name: String,
    #[arg(long, env = "PN_IP")]
    ip: String,
    #[arg(long, env = "PN_PORT", default_value_t = SERVE_PORT)]
    port: u16,
}

fn say(step: &str, ok: bool, detail: &str) {
    println!("{}", json!({"pn": step, "ok": ok, "detail": crate::report::unspaced(detail)}));
}

async fn answer(addr: std::net::SocketAddr) -> Result<String, String> {
    let go = async {
        let mut c = tokio::net::TcpStream::connect(addr).await?;
        let mut buf = String::new();
        c.read_to_string(&mut buf).await?;
        std::io::Result::Ok(buf)
    };
    match tokio::time::timeout(Duration::from_secs(5), go).await {
        Ok(Ok(b)) if b.starts_with("stormcos_qa") => Ok(format!("{addr} answered")),
        Ok(Ok(b)) => Err(format!("{addr} answered {b:?}")),
        Ok(Err(e)) => Err(format!("{addr}: {e}")),
        Err(_) => Err(format!("{addr}: no answer in 5 s")),
    }
}

pub async fn resolve(a: ResolveArgs) -> i32 {
    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let ns: Vec<&str> = resolv.lines().filter_map(|l| l.strip_prefix("nameserver")).map(str::trim).collect();
    let looked = tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((a.name.as_str(), a.port))).await;
    let addrs: Vec<std::net::SocketAddr> = match looked {
        Ok(Ok(it)) => it.collect(),
        Ok(Err(e)) => {
            say("dns", false, &format!("{} did not resolve: {e} (nameservers [{}])", a.name, ns.join(" ")));
            vec![]
        }
        Err(_) => {
            say("dns", false, &format!("{} did not resolve in 10 s (nameservers [{}])", a.name, ns.join(" ")));
            vec![]
        }
    };
    if !addrs.is_empty() {
        let ips: Vec<String> = addrs.iter().map(|s| s.ip().to_string()).collect();
        say("dns", true, &ips.join(","));
    }
    match addrs.first() {
        Some(addr) => match answer(*addr).await {
            Ok(d) => say("by-name", true, &d),
            Err(d) => say("by-name", false, &d),
        },
        None => say("by-name", false, "not tried: the name did not resolve"),
    }
    match a.ip.parse::<std::net::IpAddr>() {
        Ok(ip) => match answer((ip, a.port).into()).await {
            Ok(d) => say("by-ip", true, &d),
            Err(d) => say("by-ip", false, &d),
        },
        Err(e) => say("by-ip", false, &format!("bad ClusterIP {:?}: {e}", a.ip)),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_and_routing_failures_read_differently() {
        let ok = "warning: /proc could not be mounted\n{\"pn\":\"dns\",\"ok\":true,\"detail\":\"10.96.0.20\"}\n{\"pn\":\"by-name\",\"ok\":true,\"detail\":\"10.96.0.20:8080·answered\"}\n{\"pn\":\"by-ip\",\"ok\":true,\"detail\":\"10.96.0.20:8080·answered\"}\n";
        let n = "pn-x.ns.svc.cluster.local";
        assert_eq!(verdict(ok, "dns", n, "10.96.0.20", "Succeeded").0, Status::Pass);
        assert_eq!(verdict(ok, "by-name", n, "10.96.0.20", "Succeeded"), (Status::Pass, "10.96.0.20:8080 answered".into()));
        // DNS broken, routing fine.
        let dns_down = "{\"pn\":\"dns\",\"ok\":false,\"detail\":\"pn-x.ns.svc.cluster.local·did·not·resolve\"}\n{\"pn\":\"by-name\",\"ok\":false,\"detail\":\"not·tried\"}\n{\"pn\":\"by-ip\",\"ok\":true,\"detail\":\"10.96.0.20:8080·answered\"}\n";
        assert_eq!(verdict(dns_down, "dns", n, "10.96.0.20", "Succeeded").0, Status::Fail);
        assert_eq!(verdict(dns_down, "by-ip", n, "10.96.0.20", "Succeeded").0, Status::Pass);
        // Resolves, but to something else.
        let wrong = "{\"pn\":\"dns\",\"ok\":true,\"detail\":\"10.96.0.99\"}\n";
        assert!(verdict(wrong, "dns", n, "10.96.0.20", "Succeeded").1.contains("not the ClusterIP"));
        // Nothing from the pod at all.
        assert!(verdict("", "by-ip", n, "10.96.0.20", "Failed").1.contains("reported nothing"));
    }
}
