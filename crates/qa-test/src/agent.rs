//! Helpers a suite starts as pods from this same image.
//!
//! `serve`: a TCP listener that answers every connection — something to be
//! reached, or not.
//!
//! `agent`: probes from **inside** a namespace, where the driver cannot be
//! (an isolated namespace's pods cannot reach the apiserver, so the driver
//! stays outside and reads the agent's pod log through the API). It reads a
//! [`Plan`] from `$PLAN` and a private key from `$SSH_KEY`, probes every
//! target from itself (TCP) and from every VM over ssh (ICMP and TCP), and
//! prints one [`Probe`] per line, then `{"agent":"done"}`.
//!
//! A probe's result is what came back: `open` (connected), `refused` (a
//! reply: RST or an ICMP error), `timeout` (nothing — what a policy drop
//! looks like), or `error`.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::ssh;

pub const SERVE_PORT: u16 = 8080;

#[derive(Parser, Debug)]
#[command(name = "test serve", about = "Answer every TCP connection")]
pub struct ServeArgs {
    #[arg(long, default_value_t = SERVE_PORT)]
    port: u16,
}

pub async fn serve(a: ServeArgs) -> i32 {
    use tokio::io::AsyncWriteExt;
    let l = match tokio::net::TcpListener::bind(("0.0.0.0", a.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("serve: bind :{}: {e}", a.port);
            return 2;
        }
    };
    eprintln!("serve: listening on :{}", a.port);
    loop {
        if let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let _ = s.write_all(b"stormcos_qa\n").await;
            });
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub ip: String,
    /// TCP port to connect to; none: ICMP only.
    pub port: Option<u16>,
    /// Also ping it (from VMs; a pod has no raw socket).
    pub ping: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// This agent's own name, as `from` in its own probes.
    pub name: String,
    pub user: String,
    pub timeout_secs: u64,
    /// VMs to log in to and probe from: (name, ip).
    pub vms: Vec<(String, String)>,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Probe {
    pub from: String,
    pub to: String,
    pub proto: String,
    pub result: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Probe {
    /// Something answered: the path is open.
    pub fn reached(&self) -> bool {
        self.result == "open" || self.result == "refused"
    }
}

fn say<T: Serialize>(v: &T) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}

async fn tcp(ip: &str, port: u16, t: Duration) -> (String, String) {
    match tokio::time::timeout(t, tokio::net::TcpStream::connect((ip, port))).await {
        Err(_) => ("timeout".into(), String::new()),
        Ok(Ok(_)) => ("open".into(), String::new()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => ("refused".into(), String::new()),
        Ok(Err(e)) => {
            // EHOSTUNREACH/ENETUNREACH came back as a reply from somewhere.
            let code = e.raw_os_error();
            if matches!(code, Some(113) | Some(101)) {
                ("refused".into(), e.to_string())
            } else {
                ("error".into(), e.to_string())
            }
        }
    }
}

/// The script a VM runs: one `name proto result` line per probe.
pub fn vm_script(me: &str, targets: &[Target], timeout: u64) -> String {
    let mut s = format!(
        "p(){{ if [ \"$4\" = 1 ]; then if ping -c1 -W{timeout} \"$2\" >/dev/null 2>&1; then echo \"$1 icmp open\"; else echo \"$1 icmp timeout\"; fi; fi; \
         if [ -n \"$3\" ]; then timeout {timeout} bash -c \"</dev/tcp/$2/$3\" >/dev/null 2>&1; c=$?; \
         case $c in 0) r=open;; 124) r=timeout;; *) r=refused;; esac; echo \"$1 tcp $r\"; fi; }}\n"
    );
    for t in targets.iter().filter(|t| t.name != me) {
        s.push_str(&format!(
            "p {} {} '{}' {} &\n",
            t.name,
            t.ip,
            t.port.map(|p| p.to_string()).unwrap_or_default(),
            if t.ping { 1 } else { 0 }
        ));
    }
    s.push_str("wait\n");
    s
}

pub fn parse_vm_output(from: &str, out: &str) -> Vec<Probe> {
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (to, proto, result) = (w.next()?, w.next()?, w.next()?);
            matches!(proto, "icmp" | "tcp").then(|| Probe { from: from.into(), to: to.into(), proto: proto.into(), result: result.into(), detail: String::new() })
        })
        .collect()
}

pub async fn agent() -> i32 {
    let plan: Plan = match std::env::var("PLAN").map_err(|e| e.to_string()).and_then(|p| serde_json::from_str(&p).map_err(|e| e.to_string())) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent: $PLAN: {e}");
            return 2;
        }
    };
    let key = match std::env::var("SSH_KEY").map_err(|e| e.to_string()).and_then(|k| ssh::Key::from_openssh(&k).map_err(|e| format!("{e:#}"))) {
        Ok(k) => Arc::new(k),
        Err(e) => {
            eprintln!("agent: $SSH_KEY: {e}");
            return 2;
        }
    };
    let t = Duration::from_secs(plan.timeout_secs);

    // From this pod: TCP to every target.
    for tg in plan.targets.iter().filter(|x| x.name != plan.name) {
        if let Some(port) = tg.port {
            let (result, detail) = tcp(&tg.ip, port, t).await;
            say(&Probe { from: plan.name.clone(), to: tg.name.clone(), proto: "tcp".into(), result, detail });
        }
    }

    // From every VM, concurrently.
    let mut set = tokio::task::JoinSet::new();
    for (vm, ip) in plan.vms.clone() {
        let (key, plan) = (key.clone(), plan.clone());
        set.spawn(async move {
            let script = vm_script(&vm, &plan.targets, plan.timeout_secs);
            let budget = Duration::from_secs(plan.timeout_secs * 4 + 30);
            match ssh::run(&format!("{ip}:22"), &plan.user, key.private.clone(), &script, budget).await {
                Ok(out) => {
                    for p in parse_vm_output(&vm, &out) {
                        say(&p);
                    }
                }
                Err(e) => say(&serde_json::json!({ "from": vm, "error": format!("ssh {ip}: {e:#}") })),
            }
        });
    }
    while set.join_next().await.is_some() {}
    say(&serde_json::json!({ "agent": "done" }));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_does_not_probe_itself_and_output_parses() {
        let ts = vec![
            Target { name: "vm-1".into(), ip: "10.0.0.1".into(), port: Some(22), ping: true },
            Target { name: "srv-1".into(), ip: "10.0.0.9".into(), port: Some(8080), ping: true },
            Target { name: "lan".into(), ip: "192.168.1.1".into(), port: None, ping: true },
        ];
        let s = vm_script("vm-1", &ts, 3);
        assert!(!s.contains("p vm-1 "));
        assert!(s.contains("p srv-1 10.0.0.9 '8080' 1 &"));
        assert!(s.contains("p lan 192.168.1.1 '' 1 &"));
        let got = parse_vm_output("vm-1", "srv-1 icmp open\nsrv-1 tcp timeout\nnoise\n");
        assert_eq!(got.len(), 2);
        assert!(got[0].reached() && !got[1].reached());
    }
}
