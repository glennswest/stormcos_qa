//! `/test claim` — the workload of `long`'s container waves (#17): one pod of
//! a Deployment, with a stormblock claim mounted at `--data`.
//!
//! On start it looks at its claim. If the claim is empty, it writes the run's
//! token and a 1 MiB blob derived from it, syncs them, and logs
//! `{"claim":"written"}`. If they are there, it checks both, and logs
//! `{"claim":"found"}` or `{"claim":"mismatch",…}`. The driver reads these
//! lines through the pod log API, so proving that a claim kept its data
//! across a restart or a reschedule needs no pod network.
//!
//! Then it answers every TCP connection on `--port` (the readiness probe and
//! the Service), and with `--exit-once-after N` it exits once, N seconds
//! after it starts, so the kubelet restarts it in place. A marker on the claim
//! makes it once per claim: neither the restarted container nor a pod that
//! replaces this one exits again.
//!
//! Its line also carries `sha256`, the digest of the claim's file set (each
//! file's name and bytes, by name), so `/test mobility` (#84) can say the
//! bytes on node B equal the bytes written on node A.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use serde_json::json;

use crate::agent::SERVE_PORT;

#[derive(Parser, Debug)]
#[command(name = "test claim", about = "Container-wave workload: write or verify a claim, serve, exit once")]
pub struct Args {
    /// What must be on the claim (the driver's, per Deployment).
    #[arg(long, env = "CLAIM_TOKEN")]
    token: String,
    #[arg(long, env = "CLAIM_DATA", default_value = "/data")]
    data: PathBuf,
    #[arg(long, default_value_t = SERVE_PORT)]
    port: u16,
    /// Exit once, this many seconds after start (0: never).
    #[arg(long, env = "CLAIM_EXIT_ONCE_AFTER", default_value_t = 0)]
    exit_once_after: u64,
}

const BLOB: usize = 1 << 20;

/// The blob for a token: its bytes repeated, so any corruption shows.
pub fn blob(token: &str) -> Vec<u8> {
    let t = if token.is_empty() { b"-".as_slice() } else { token.as_bytes() };
    t.iter().copied().cycle().take(BLOB).collect()
}

/// What the claim holds: `written` (it was empty), `found`, or a mismatch.
pub fn check_or_write(data: &Path, token: &str) -> std::io::Result<(&'static str, String)> {
    let tok = data.join("token");
    let bl = data.join("blob");
    match std::fs::read_to_string(&tok) {
        Ok(t) => {
            if t != token {
                return Ok(("mismatch", format!("token on the claim is {t:?}, expected {token:?}")));
            }
            match std::fs::read(&bl) {
                Ok(b) if b == blob(token) => Ok(("found", format!("token and {} byte blob intact", b.len()))),
                Ok(b) => Ok(("mismatch", format!("blob is {} bytes and differs from what was written", b.len()))),
                Err(e) => Ok(("mismatch", format!("token present, blob unreadable: {e}"))),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_synced(&bl, &blob(token))?;
            write_synced(&tok, token.as_bytes())?;
            Ok(("written", format!("token and {BLOB} byte blob")))
        }
        Err(e) => Err(e),
    }
}

/// The sha256 of the claim's file set: `token` and `blob`, each as its name,
/// a NUL, its length and its bytes, in that order. The exit marker is not
/// data and is left out.
pub fn fileset_sha256(data: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for name in ["blob", "token"] {
        let b = std::fs::read(data.join(name))?;
        h.update(name.as_bytes());
        h.update([0]);
        h.update((b.len() as u64).to_le_bytes());
        h.update(&b);
    }
    Ok(crate::sqlite::hex(&h.finalize()))
}

fn write_synced(p: &Path, b: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(p)?;
    f.write_all(b)?;
    f.sync_all()
}

/// One JSON line with no spaces (`·` in the detail): rustkube-node's `/log`
/// cuts the first three words of a line with three or more spaces
/// (rustkube-node#136), which left the driver nothing to parse (#17).
fn say(v: serde_json::Value) {
    println!("{}", crate::report::unspaced(&v.to_string()));
}

pub async fn main(a: Args) -> i32 {
    match check_or_write(&a.data, &a.token) {
        Ok((state, detail)) => {
            let sha = fileset_sha256(&a.data).unwrap_or_else(|e| format!("unreadable: {e}"));
            say(json!({"claim": state, "detail": detail, "sha256": sha}))
        }
        Err(e) => {
            say(json!({"claim": "error", "detail": format!("{}: {e}", a.data.display())}));
            return 2;
        }
    }
    let l = match tokio::net::TcpListener::bind(("0.0.0.0", a.port)).await {
        Ok(l) => l,
        Err(e) => {
            say(json!({"claim": "error", "detail": format!("bind :{}: {e}", a.port)}));
            return 2;
        }
    };
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                let msg = format!("stormcos_qa claim {host}\n");
                tokio::spawn(async move {
                    let _ = s.write_all(msg.as_bytes()).await;
                });
            }
        }
    });
    let marker = a.data.join("exited-once");
    if a.exit_once_after > 0 && !marker.exists() {
        tokio::time::sleep(Duration::from_secs(a.exit_once_after)).await;
        if let Err(e) = write_synced(&marker, b"1") {
            say(json!({"claim": "error", "detail": format!("marking the exit: {e}")}));
        }
        say(json!({"claim": "exiting", "detail": "once, for an in-place restart"}));
        return 1;
    }
    std::future::pending::<()>().await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_then_found_then_mismatch() {
        let d = std::env::temp_dir().join(format!("qa-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(check_or_write(&d, "tok-1").unwrap().0, "written");
        let sha = fileset_sha256(&d).unwrap();
        assert_eq!(sha.len(), 64);
        assert_eq!(check_or_write(&d, "tok-1").unwrap().0, "found");
        std::fs::write(d.join("exited-once"), b"1").unwrap();
        assert_eq!(fileset_sha256(&d).unwrap(), sha, "the marker is not data");
        assert_eq!(check_or_write(&d, "tok-2").unwrap().0, "mismatch");
        std::fs::write(d.join("blob"), b"short").unwrap();
        assert_eq!(check_or_write(&d, "tok-1").unwrap().0, "mismatch");
        assert_ne!(fileset_sha256(&d).unwrap(), sha);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn blob_is_a_mebibyte_of_the_token() {
        let b = blob("ab");
        assert_eq!(b.len(), BLOB);
        assert_eq!(&b[..4], b"abab");
        assert_eq!(blob("").len(), BLOB);
    }
}
