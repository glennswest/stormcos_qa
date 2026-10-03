//! The workloads of `/test turbomode` (#26), run by its pods from this same
//! image (it is scratch, so there is no `sleep` or `python3` to run):
//!
//! - `/test sleep [secs]` — the sleeping profile: sleep, exit 0;
//! - `/test sqlite` — the SQLite profile, on the pod's fresh claim at
//!   `--data`: write 1,000 records into a new database, read them back and
//!   check them (`PRAGMA integrity_check` too), log `{"phase":"verified"}`,
//!   sleep, check again, log `{"phase":"complete"}`. Each record's content is
//!   derived from the Pod's UID, so two Pods sharing a claim show. Any error
//!   logs `{"phase":"error"}` and exits 1.
//!
//! A port of `tools/turbomode/sqlite-workload.py`; the driver checks the log
//! with [`evidence_problem`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const RECORDS: usize = 1000;

#[derive(Parser, Debug)]
#[command(name = "test sleep", about = "turbomode's sleeping-profile workload: sleep, exit 0")]
pub struct SleepArgs {
    #[arg(default_value_t = 120)]
    seconds: u64,
}

pub async fn sleep(a: SleepArgs) -> i32 {
    tokio::time::sleep(Duration::from_secs(a.seconds)).await;
    0
}

#[derive(Parser, Debug)]
#[command(name = "test sqlite", about = "turbomode's SQLite workload: write, verify, sleep, verify")]
pub struct Args {
    /// The Pod's UID (downward API): what every record is derived from.
    #[arg(long, env = "POD_UID")]
    pod_uid: String,
    /// The claim's mount.
    #[arg(long, env = "SQLITE_DATA", default_value = "/data")]
    data: PathBuf,
    /// Seconds between the two checks.
    #[arg(long, env = "SQLITE_SLEEP", default_value_t = 120)]
    sleep: u64,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// Record `i`'s payload for a Pod: different per Pod, 1 KiB.
pub fn expected(identity: &str, record: usize) -> String {
    sha256(&format!("{identity}:{record}")).repeat(16)
}

/// Create the database (it must not exist: the claim is fresh) and write
/// every record in one transaction, fully synced.
pub fn write(path: &Path, identity: &str) -> Result<(), String> {
    if path.exists() {
        return Err("unexpected database on fresh PVC".into());
    }
    let e = |e: rusqlite::Error| e.to_string();
    let mut db = Connection::open(path).map_err(e)?;
    db.pragma_update(None, "journal_mode", "DELETE").map_err(e)?;
    db.pragma_update(None, "synchronous", "FULL").map_err(e)?;
    db.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, payload TEXT NOT NULL, digest TEXT NOT NULL)", []).map_err(e)?;
    let tx = db.transaction().map_err(e)?;
    for i in 0..RECORDS {
        let payload = expected(identity, i);
        let digest = sha256(&payload);
        tx.execute("INSERT INTO records VALUES (?1, ?2, ?3)", rusqlite::params![i as i64, payload, digest]).map_err(e)?;
    }
    tx.commit().map_err(e)?;
    db.close().map_err(|(_, err)| err.to_string())
}

/// Read every record back, read-only: content, per-record digest, count and
/// SQLite's own integrity check. The checksum over all payloads.
pub fn verify(path: &Path, identity: &str) -> Result<String, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(e)?;
    let mut st = db.prepare("SELECT id, payload, digest FROM records ORDER BY id").map_err(e)?;
    let rows: Vec<(i64, String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(e)?
        .collect::<Result<_, _>>()
        .map_err(e)?;
    if rows.len() != RECORDS {
        return Err(format!("expected {RECORDS} records, got {}", rows.len()));
    }
    let mut sum = Sha256::new();
    for (i, (id, payload, digest)) in rows.iter().enumerate() {
        if *id != i as i64 || *payload != expected(identity, i) {
            return Err(format!("record {i} content mismatch"));
        }
        if *digest != sha256(payload) {
            return Err(format!("record {i} checksum mismatch"));
        }
        sum.update(payload.as_bytes());
    }
    let integrity: Vec<String> = db
        .prepare("PRAGMA integrity_check")
        .map_err(e)?
        .query_map([], |r| r.get(0))
        .map_err(e)?
        .collect::<Result<_, _>>()
        .map_err(e)?;
    if integrity != ["ok"] {
        return Err(format!("integrity_check failed: {integrity:?}"));
    }
    Ok(hex(&sum.finalize()))
}

fn say(v: Value) {
    println!("{v}");
}

pub async fn main(a: Args) -> i32 {
    let path = a.data.join("records.sqlite");
    let id = a.pod_uid.clone();
    let fail = |why: String| {
        say(json!({"phase": "error", "pod_uid": id, "error": why}));
        1
    };
    let begin = Instant::now();
    if let Err(why) = write(&path, &a.pod_uid) {
        return fail(why);
    }
    let checksum = match verify(&path, &a.pod_uid) {
        Ok(c) => c,
        Err(why) => return fail(why),
    };
    say(json!({"phase": "verified", "pod_uid": a.pod_uid, "records": RECORDS, "sha256": checksum,
        "write_read_seconds": begin.elapsed().as_secs_f64()}));
    let slept = Instant::now();
    tokio::time::sleep(Duration::from_secs(a.sleep)).await;
    let after = match verify(&path, &a.pod_uid) {
        Ok(c) => c,
        Err(why) => return fail(why),
    };
    if after != checksum {
        return fail("database changed during sleep".into());
    }
    say(json!({"phase": "complete", "pod_uid": a.pod_uid, "records": RECORDS, "sha256": after,
        "sleep_seconds": slept.elapsed().as_secs_f64()}));
    0
}

/// Why a Pod's log is not valid SQLite evidence for Pod `uid`, or `None`
/// when it is (both phases, this Pod's, 1,000 records, the same checksum
/// across a sleep of at least `min_sleep` seconds). The workload writes JSON
/// objects only; other lines are the runtime's (stormpump warns on stderr,
/// and the kubelet's `/log` may cut their start, rustkube-node#136) and are
/// skipped. A line that starts as an object but does not parse is a fail.
pub fn evidence_problem(log: &str, uid: &str, min_sleep: u64) -> Option<String> {
    let mut entries = Vec::new();
    for l in log.lines().filter(|l| l.trim_start().starts_with('{')) {
        match serde_json::from_str::<Value>(l) {
            Ok(v) => entries.push(v),
            Err(_) => return Some("log is not JSON lines (workload error?)".into()),
        }
    }
    if let Some(e) = entries.iter().find(|e| e["phase"] == "error") {
        return Some(format!("workload error: {}", e["error"]));
    }
    let phase = |p: &str| entries.iter().find(|e| e["phase"] == p);
    let (Some(verified), Some(complete)) = (phase("verified"), phase("complete")) else {
        return Some("missing verified/complete evidence".into());
    };
    if complete["pod_uid"] != uid || verified["pod_uid"] != uid {
        return Some("evidence belongs to another Pod".into());
    }
    if complete["records"] != RECORDS || verified["records"] != RECORDS {
        return Some(format!("record count is not {RECORDS}"));
    }
    if complete["sha256"] != verified["sha256"] {
        return Some("checksum changed across the sleep".into());
    }
    if complete["sleep_seconds"].as_f64().unwrap_or(0.0) < min_sleep as f64 {
        return Some(format!("slept less than {min_sleep} seconds"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qa-sqlite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_verifies_and_refuses_a_used_claim() {
        let d = dir("ok");
        let db = d.join("records.sqlite");
        write(&db, "uid-1").unwrap();
        let sum = verify(&db, "uid-1").unwrap();
        assert_eq!(sum, verify(&db, "uid-1").unwrap());
        // Another Pod's identity: every payload differs.
        assert!(verify(&db, "uid-2").unwrap_err().contains("content mismatch"));
        assert!(write(&db, "uid-1").unwrap_err().contains("fresh PVC"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_record_is_caught() {
        let d = dir("bad");
        let db = d.join("records.sqlite");
        write(&db, "uid-1").unwrap();
        Connection::open(&db).unwrap().execute("UPDATE records SET digest = 'x' WHERE id = 7", []).unwrap();
        assert_eq!(verify(&db, "uid-1").unwrap_err(), "record 7 checksum mismatch");
        Connection::open(&db).unwrap().execute("DELETE FROM records WHERE id = 999", []).unwrap();
        assert!(verify(&db, "uid-1").unwrap_err().contains("got 999"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn evidence() {
        let v = |uid: &str, sum: &str| json!({"phase":"verified","pod_uid":uid,"records":1000,"sha256":sum}).to_string();
        let c = |uid: &str, sum: &str, s: f64| {
            json!({"phase":"complete","pod_uid":uid,"records":1000,"sha256":sum,"sleep_seconds":s}).to_string()
        };
        let good = format!("{}\n{}\n", v("u", "a"), c("u", "a", 120.2));
        assert_eq!(evidence_problem(&good, "u", 120), None);
        assert!(evidence_problem(&good, "other", 120).unwrap().contains("another Pod"));
        assert!(evidence_problem(&format!("{}\n{}", v("u", "a"), c("u", "b", 121.0)), "u", 120).unwrap().contains("checksum"));
        assert!(evidence_problem(&format!("{}\n{}", v("u", "a"), c("u", "a", 3.0)), "u", 120).unwrap().contains("slept less"));
        assert!(evidence_problem(&v("u", "a"), "u", 120).unwrap().contains("missing"));
        assert!(evidence_problem("{\"phase\": \"verif", "u", 120).unwrap().contains("not JSON"));
        assert!(evidence_problem("Traceback", "u", 120).unwrap().contains("missing"));
        // The runtime's line on the pod's stderr, cut by the kubelet's /log (#26).
        let warned = format!("not be mounted (is the directory in the image?)\n{good}");
        assert_eq!(evidence_problem(&warned, "u", 120), None);
        let err = json!({"phase":"error","pod_uid":"u","error":"disk I/O error"}).to_string();
        assert!(evidence_problem(&format!("{}\n{err}", v("u", "a")), "u", 120).unwrap().contains("disk I/O"));
    }
}
