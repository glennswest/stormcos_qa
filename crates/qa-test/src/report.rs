//! stdout, one JSON object per line (stormcentral docs/test-standard.md):
//! `{"test","status","ms","detail"}` per test, then `{"summary":{…}}`.
//! Every line is also appended to `<results>/<name>.jsonl`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Serialize)]
pub struct Line {
    pub test: String,
    pub status: Status,
    pub ms: u64,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave: Option<Value>,
}

impl Line {
    pub fn new(test: impl Into<String>, status: Status, took: Duration, detail: impl Into<String>) -> Self {
        Line { test: test.into(), status, ms: took.as_millis() as u64, detail: detail.into(), wave: None }
    }
}

#[derive(Default)]
struct Counts {
    pass: usize,
    fail: usize,
    skip: usize,
    /// Fail lines that mean "could not run" (infrastructure), not "failed".
    infra: usize,
}

pub struct Out {
    file: Option<PathBuf>,
    counts: Mutex<Counts>,
}

impl Out {
    pub fn new(results: &std::path::Path, name: &str) -> Self {
        let file = std::fs::create_dir_all(results).ok().map(|_| results.join(format!("{name}.jsonl")));
        Out { file, counts: Mutex::new(Counts::default()) }
    }

    fn write(&self, v: &str) {
        println!("{v}");
        if let Some(f) = &self.file
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(f)
        {
            let _ = writeln!(f, "{v}");
        }
    }

    pub fn emit(&self, l: Line) {
        {
            let mut c = self.counts.lock().unwrap();
            match l.status {
                Status::Pass => c.pass += 1,
                Status::Fail => c.fail += 1,
                Status::Skip => c.skip += 1,
            }
        }
        let line = serde_json::to_string(&l).unwrap_or_default();
        self.write(&line);
        // rustkube-node's `/log` cuts the first three words of a line with
        // three or more spaces (rustkube-node#136), which takes the test's
        // name and status: a copy with no spaces goes to stderr.
        if line.matches(' ').count() >= 3 {
            eprintln!("{}", unspaced(&line));
        }
    }

    pub fn failed(&self) -> usize {
        self.counts.lock().unwrap().fail
    }

    /// A test that could not run (infrastructure, a missing right): a Fail
    /// line reading `could not run: …`, counted apart so the exit says 2.
    pub fn could_not_run(&self, test: impl Into<String>, took: Duration, why: impl std::fmt::Display) {
        self.counts.lock().unwrap().infra += 1;
        self.emit(Line::new(test, Status::Fail, took, format!("could not run: {why}")));
    }

    /// A test that ended in `e`: "could not run" when `e` is an
    /// infrastructure failure the retry helper gave up on (#57), else a
    /// failure. `detail` is the line's text (the error is in it).
    pub fn failure(&self, test: impl Into<String>, took: Duration, e: &anyhow::Error, detail: impl Into<String>) {
        if retry::is_infra(e) {
            self.could_not_run(test, took, detail.into());
        } else {
            self.emit(Line::new(test, Status::Fail, took, detail));
        }
    }

    /// Lines counted as "could not run".
    pub fn infra(&self) -> usize {
        self.counts.lock().unwrap().infra
    }

    /// 1 when something really failed; else 2 when something could not
    /// run; else 0.
    pub fn exit_code(&self) -> i32 {
        let c = self.counts.lock().unwrap();
        if c.fail > c.infra {
            1
        } else if c.infra > 0 {
            2
        } else {
            0
        }
    }

    pub fn summary(&self) {
        let c = self.counts.lock().unwrap();
        let s = serde_json::json!({ "summary": { "pass": c.pass, "fail": c.fail, "skip": c.skip } });
        drop(c);
        self.write(&s.to_string());
    }
}

/// A line with its spaces as `·`, so a log reader that splits on spaces
/// keeps all of it (rustkube-node#136).
pub fn unspaced(line: &str) -> String {
    line.replace(' ', "\u{b7}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infrastructure_is_could_not_run_not_failed() {
        let dir = std::env::temp_dir().join(format!("qa-report-{}", std::process::id()));
        let out = Out::new(&dir, "t");
        let gave: anyhow::Error = retry::Infra { what: "GET /x".into(), attempts: 5, elapsed: Duration::from_secs(9), last: "timeout".into(), unsure: false }.into();
        out.failure("a", Duration::ZERO, &gave.context("listing"), "listing: GET /x gave up");
        assert_eq!((out.failed(), out.infra(), out.exit_code()), (1, 1, 2));
        out.failure("b", Duration::ZERO, &anyhow::anyhow!("403 forbidden"), "403");
        assert_eq!((out.failed(), out.infra(), out.exit_code()), (2, 1, 1));
        let _ = std::fs::remove_dir_all(dir);
    }
}
