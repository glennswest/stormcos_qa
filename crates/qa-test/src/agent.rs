//! Helpers a suite starts as pods from this same image.
//!
//! `serve`: a TCP listener that answers every connection — something to be
//! reached, or not.
//!
//! `agent`: probes from **inside** a namespace, where the driver cannot be
//! (an isolated namespace's pods cannot reach the apiserver, so the driver
//! stays outside and reads the agent's pod log through the API). It reads a
//! [`Plan`] from `$PLAN` and a private key from `$SSH_KEY` (needed only with
//! VMs in the plan), probes every target from itself (TCP) and from every VM
//! over ssh (ICMP and TCP), and prints one [`Probe`] per line, then `{"agent":"done"}`.
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
    /// Tries it took (#57): a probe without an answer is retried
    /// (`retry::Policy::PROBE`); the result is the last try's.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub attempts: u32,
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
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

/// A TCP probe, retried while nothing answers: (result, detail, attempts).
pub async fn tcp(ip: &str, port: u16, t: Duration) -> (String, String, u32) {
    let ((result, detail), n) = retry::probe(&retry::Policy::PROBE, &format!("tcp {ip}:{port}"), |_| async {
        let r = tcp_once(ip, port, t).await;
        let answered = r.0 == "open" || r.0 == "refused";
        (r, answered)
    })
    .await;
    (result, detail, n)
}

async fn tcp_once(ip: &str, port: u16, t: Duration) -> (String, String) {
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

/// The script a VM runs: one `name proto result attempts` line per probe.
/// A probe without an answer is tried again, 3 tries 1 s then 2 s apart
/// (as `retry::Policy::PROBE`, #57).
pub fn vm_script(me: &str, targets: &[Target], timeout: u64) -> String {
    let mut s = format!(
        "p(){{ if [ \"$4\" = 1 ]; then n=0; r=timeout; while [ $n -lt 3 ]; do n=$((n+1)); \
         if ping -c1 -W{timeout} \"$2\" >/dev/null 2>&1; then r=open; break; fi; [ $n -lt 3 ] && sleep $n; done; echo \"$1 icmp $r $n\"; fi; \
         if [ -n \"$3\" ]; then n=0; while [ $n -lt 3 ]; do n=$((n+1)); timeout {timeout} bash -c \"</dev/tcp/$2/$3\" >/dev/null 2>&1; c=$?; \
         case $c in 0) r=open;; 124) r=timeout;; *) r=refused;; esac; [ $r != timeout ] && break; [ $n -lt 3 ] && sleep $n; done; echo \"$1 tcp $r $n\"; fi; }}\n"
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
            let attempts = w.next().and_then(|n| n.parse().ok()).unwrap_or(1);
            matches!(proto, "icmp" | "tcp").then(|| Probe { from: from.into(), to: to.into(), proto: proto.into(), result: result.into(), detail: String::new(), attempts })
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
    // The key is for the VMs: an agent with none to log in to needs none
    // (medium's host-network node probe, #23).
    let key = match std::env::var("SSH_KEY").map_err(|e| e.to_string()).and_then(|k| ssh::Key::from_openssh(&k).map_err(|e| format!("{e:#}"))) {
        Ok(k) => Some(Arc::new(k)),
        Err(_) if plan.vms.is_empty() => None,
        Err(e) => {
            eprintln!("agent: $SSH_KEY: {e}");
            return 2;
        }
    };
    let t = Duration::from_secs(plan.timeout_secs);

    // From this pod: TCP to every target, concurrently (each may retry).
    let mut set = tokio::task::JoinSet::new();
    for tg in plan.targets.iter().filter(|x| x.name != plan.name).cloned() {
        let from = plan.name.clone();
        if let Some(port) = tg.port {
            set.spawn(async move {
                let (result, detail, attempts) = tcp(&tg.ip, port, t).await;
                Probe { from, to: tg.name, proto: "tcp".into(), result, detail, attempts }
            });
        }
    }
    while let Some(p) = set.join_next().await {
        if let Ok(p) = p {
            say(&p);
        }
    }

    // From every VM, concurrently.
    let mut set = tokio::task::JoinSet::new();
    for (vm, ip) in plan.vms.clone() {
        // Some whenever there are VMs (checked above).
        let Some(key) = key.clone() else { break };
        let plan = plan.clone();
        set.spawn(async move {
            let script = vm_script(&vm, &plan.targets, plan.timeout_secs);
            // icmp and tcp, 3 tries each, 1 s + 2 s between (vm_script).
            let budget = Duration::from_secs(plan.timeout_secs * 6 + 6 + 30);
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
        let got = parse_vm_output("vm-1", "srv-1 icmp open 2\nsrv-1 tcp timeout 3\nnoise\nlan icmp open\n");
        assert_eq!(got.len(), 3);
        assert!(got[0].reached() && !got[1].reached());
        assert_eq!((got[0].attempts, got[1].attempts, got[2].attempts), (2, 3, 1));
        assert!(s.contains("while [ $n -lt 3 ]"));
        assert!(!serde_json::to_string(&got[2]).unwrap().contains("attempts"), "1 attempt is not written");
    }
}
