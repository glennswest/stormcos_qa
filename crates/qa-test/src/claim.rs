//! `/test claim` — the workload of `long`'s container waves (#17): one pod of
//! a Deployment, with a stormblock claim mounted at `--data`.
//!
//! On start it looks at its claim. If the token and blob are there, it checks
//! both, and logs `{"claim":"found"}` or `{"claim":"mismatch",…}`. If the
//! claim is empty, what it does depends on whether this container may
//! initialize it (#97, finding dcce5bf00b54):
//!
//! - **init**: `--init` (`CLAIM_INIT`, default on) and no earlier container
//!   of this pod in `--state`. It writes the run's token and a 1 MiB blob
//!   derived from it, syncs them, and logs `{"claim":"written"}`.
//! - **verify only**: every other start: a container restarted in place (an
//!   earlier one is on record in `--state`), or a pod started with
//!   `CLAIM_INIT=0`. An empty claim is then `{"claim":"missing"}`, a failure:
//!   the data was lost, and writing it again would hide that.
//!
//! The authority to initialize is kept outside the tested claim: the
//! driver's switch (a ConfigMap key it turns off once the wave has written)
//! and `--state`, an emptyDir that outlives a container restart but not the
//! pod. In `--state` each container records what it saw, and its line
//! carries the earlier ones (`earlier`); a restarted container whose
//! predecessor reported a failure reports `mismatch` too, so a restart does
//! not wipe the evidence from the log the driver reads.
//!
//! The driver reads these lines through the pod log API, so proving that a
//! claim kept its data across a restart or a reschedule needs no pod network.
//!
//! Then it answers every TCP connection on `--port` (the readiness probe and
//! the Service), and with `--exit-once-after N` the container that wrote the
//! claim exits, N seconds after it starts, so the kubelet restarts it in
//! place. Only an initializing container exits, so neither the restarted
//! container nor a replacement pod exits again, and nothing on the claim
//! decides it. `--exit-once-after` needs `--state`.
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
    /// Exit once, this many seconds after start (0: never). Only a
    /// container that wrote the claim exits; needs `--state`.
    #[arg(long, env = "CLAIM_EXIT_ONCE_AFTER", default_value_t = 0)]
    exit_once_after: u64,
    /// May this pod initialize an empty claim (`1`/`0`)? A restarted
    /// container never does, whatever this says.
    #[arg(long, env = "CLAIM_INIT", default_value = "1", value_parser = clap::builder::BoolishValueParser::new())]
    init: bool,
    /// A directory outside the claim that outlives a container restart but
    /// not the pod (an emptyDir): each container's result, one per line.
    #[arg(long, env = "CLAIM_STATE")]
    state: Option<PathBuf>,
}

const BLOB: usize = 1 << 20;

/// The blob for a token: its bytes repeated, so any corruption shows.
pub fn blob(token: &str) -> Vec<u8> {
    let t = if token.is_empty() { b"-".as_slice() } else { token.as_bytes() };
    t.iter().copied().cycle().take(BLOB).collect()
}

/// What the claim holds: `written` (it was empty and `may_init`), `found`,
/// `missing` (empty, and this start may not write it), or a mismatch.
pub fn check_or_write(data: &Path, token: &str, may_init: bool) -> std::io::Result<(&'static str, String)> {
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !may_init => {
            let blob_too = if bl.exists() { "token absent (the blob is there)" } else { "token and blob absent" };
            Ok(("missing", format!("{blob_too}: the claim lost its data, and this start only verifies")))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_synced(&bl, &blob(token))?;
            write_synced(&tok, token.as_bytes())?;
            Ok(("written", format!("token and {BLOB} byte blob")))
        }
        Err(e) => Err(e),
    }
}

/// What earlier containers of this pod reported, oldest first (none: this
/// is the pod's first container, or there is no `--state`).
pub fn earlier(state: Option<&Path>) -> std::io::Result<Vec<String>> {
    let Some(d) = state else { return Ok(Vec::new()) };
    match std::fs::read_to_string(d.join("incarnations")) {
        Ok(s) => Ok(s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Add this container's result to the pod's record, synced.
pub fn record(state: &Path, result: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(state.join("incarnations"))?;
    writeln!(f, "{result}")?;
    f.sync_all()
}

/// This start's verdict: the claim's state, made a `mismatch` when an
/// earlier container of the pod reported anything but `written`/`found`
/// (its line is gone from the log once the container restarts).
pub fn verdict(state: &'static str, detail: String, earlier: &[String]) -> (&'static str, String) {
    match earlier.iter().find(|e| *e != "written" && *e != "found") {
        Some(bad) if state == "written" || state == "found" => ("mismatch", format!("{detail}, but an earlier container of this pod reported {bad}")),
        _ => (state, detail),
    }
}

/// The sha256 of the claim's file set: `token` and `blob`, each as its name,
/// a NUL, its length and its bytes, in that order. Nothing else on the
/// claim (an old `exited-once` marker) is data.
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

/// What one container start saw and recorded.
#[derive(Debug, PartialEq)]
pub struct Start {
    pub state: &'static str,
    pub detail: String,
    /// It was allowed to initialize an empty claim.
    pub may_init: bool,
    /// What earlier containers of this pod reported.
    pub earlier: Vec<String>,
}

/// One container start: read the pod's record, check (or, only as the
/// pod's first container with `init`, write) the claim, record the result.
/// `Err` when the record cannot be read or written.
pub fn start(data: &Path, token: &str, init: bool, state: Option<&Path>) -> Result<Start, String> {
    let earlier = earlier(state).map_err(|e| format!("reading this pod's record in {state:?}: {e}"))?;
    let may_init = init && earlier.is_empty();
    let (st, detail) = match check_or_write(data, token, may_init) {
        Ok((st, detail)) => verdict(st, detail, &earlier),
        Err(e) => ("error", format!("{}: {e}", data.display())),
    };
    if let Some(d) = state {
        record(d, st).map_err(|e| format!("recording {st} in {}: {e}", d.display()))?;
    }
    Ok(Start { state: st, detail, may_init, earlier })
}

/// One JSON line with no spaces (`·` in the detail): rustkube-node's `/log`
/// cuts the first three words of a line with three or more spaces
/// (rustkube-node#136), which left the driver nothing to parse (#17).
fn say(v: serde_json::Value) {
    println!("{}", crate::report::unspaced(&v.to_string()));
}

pub async fn main(a: Args) -> i32 {
    if a.exit_once_after > 0 && a.state.is_none() {
        say(json!({"claim": "error", "detail": "--exit-once-after needs --state: without it a restarted container cannot tell it is one"}));
        return 2;
    }
    let s = match start(&a.data, &a.token, a.init, a.state.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            say(json!({"claim": "error", "detail": e}));
            return 2;
        }
    };
    let (state, detail, may_init, earlier) = (s.state, s.detail, s.may_init, s.earlier);
    let sha = fileset_sha256(&a.data).unwrap_or_else(|e| format!("unreadable: {e}"));
    say(json!({"claim": state, "detail": detail, "sha256": sha, "mode": if may_init { "init" } else { "verify" }, "earlier": earlier}));
    if state == "error" {
        return 2;
    }
    // A mismatch or a missing claim keeps serving: the pod comes Ready and
    // the driver reads the failure from its log.
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
    if a.exit_once_after > 0 && state == "written" {
        tokio::time::sleep(Duration::from_secs(a.exit_once_after)).await;
        say(json!({"claim": "exiting", "detail": "once, for an in-place restart"}));
        return 1;
    }
    std::future::pending::<()>().await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qa-claim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn written_then_found_then_mismatch() {
        let d = dir("cow");
        assert_eq!(check_or_write(&d, "tok-1", true).unwrap().0, "written");
        let sha = fileset_sha256(&d).unwrap();
        assert_eq!(sha.len(), 64);
        assert_eq!(check_or_write(&d, "tok-1", true).unwrap().0, "found");
        assert_eq!(check_or_write(&d, "tok-1", false).unwrap().0, "found");
        std::fs::write(d.join("exited-once"), b"1").unwrap();
        assert_eq!(fileset_sha256(&d).unwrap(), sha, "an old marker is not data");
        assert_eq!(check_or_write(&d, "tok-2", true).unwrap().0, "mismatch");
        std::fs::write(d.join("blob"), b"short").unwrap();
        assert_eq!(check_or_write(&d, "tok-1", true).unwrap().0, "mismatch");
        assert_ne!(fileset_sha256(&d).unwrap(), sha);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn verify_only_never_writes_an_empty_claim() {
        let d = dir("verify");
        let (st, detail) = check_or_write(&d, "tok", false).unwrap();
        assert_eq!(st, "missing", "{detail}");
        assert!(!d.join("token").exists() && !d.join("blob").exists(), "nothing written");
        std::fs::write(d.join("blob"), blob("tok")).unwrap();
        let (st, detail) = check_or_write(&d, "tok", false).unwrap();
        assert_eq!(st, "missing");
        assert!(detail.contains("the blob is there"), "{detail}");
        assert!(!d.join("token").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn wipe(claim: &Path) {
        for f in ["token", "blob", "exited-once"] {
            let _ = std::fs::remove_file(claim.join(f));
        }
    }

    /// Finding dcce5bf00b54: the claim loses everything between starts. A
    /// container restarted in place (same `--state`) and a replacement pod
    /// (fresh `--state`, the driver's switch off) both report `missing`
    /// and write nothing, and the restarted one keeps saying so after its
    /// own restart.
    #[test]
    fn a_lost_claim_is_reported_not_rewritten() {
        let claim = dir("lost-claim");
        let pod1 = dir("lost-pod1");
        let first = start(&claim, "tok", true, Some(&pod1)).unwrap();
        assert_eq!((first.state, first.may_init, first.earlier.len()), ("written", true, 0));

        // Restart in place: the claim intact is found, verify only.
        let again = start(&claim, "tok", true, Some(&pod1)).unwrap();
        assert_eq!((again.state, again.may_init), ("found", false));
        assert_eq!(again.earlier, ["written"]);

        // Restart in place after a total loss: init is still on, but an
        // earlier container is on record.
        wipe(&claim);
        let lost = start(&claim, "tok", true, Some(&pod1)).unwrap();
        assert_eq!((lost.state, lost.may_init), ("missing", false));
        assert!(!claim.join("token").exists(), "not rewritten");
        // …and the container after that does not hide it.
        let later = start(&claim, "tok", true, Some(&pod1)).unwrap();
        assert_eq!(later.state, "missing");
        assert_eq!(later.earlier, ["written", "found", "missing"]);

        // A replacement pod: fresh emptyDir, the wave's switch off.
        let pod2 = dir("lost-pod2");
        let repl = start(&claim, "tok", false, Some(&pod2)).unwrap();
        assert_eq!((repl.state, repl.may_init), ("missing", false));
        assert!(!claim.join("token").exists(), "not rewritten");

        // An earlier failure survives a restart even if the claim now reads
        // back (here: rewritten behind the pod's back).
        assert!(check_or_write(&claim, "tok", true).unwrap().0 == "written");
        let after = start(&claim, "tok", false, Some(&pod2)).unwrap();
        assert_eq!(after.state, "mismatch");
        assert!(after.detail.contains("reported missing"), "{}", after.detail);
        for d in [claim, pod1, pod2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn without_a_state_dir_init_follows_the_switch() {
        let claim = dir("nostate");
        assert_eq!(start(&claim, "tok", false, None).unwrap().state, "missing");
        assert_eq!(start(&claim, "tok", true, None).unwrap().state, "written");
        assert_eq!(start(&claim, "tok", false, None).unwrap().state, "found");
        let _ = std::fs::remove_dir_all(&claim);
    }

    #[test]
    fn an_unwritable_record_is_an_error() {
        let claim = dir("norec");
        let r = start(&claim, "tok", true, Some(&claim.join("absent/dir")));
        assert!(r.unwrap_err().contains("recording written"));
        let _ = std::fs::remove_dir_all(&claim);
    }

    #[test]
    fn blob_is_a_mebibyte_of_the_token() {
        let b = blob("ab");
        assert_eq!(b.len(), BLOB);
        assert_eq!(&b[..4], b"abab");
        assert_eq!(blob("").len(), BLOB);
    }
}
