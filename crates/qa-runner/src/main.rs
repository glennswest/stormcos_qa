//! qa-runner — discover and run stormcos QA tests against a built release
//! and/or a live test cluster, file a GitHub issue in each failing test's
//! owning repo (deduplicated), and emit a report the builder uses to tombstone
//! a release when a blocking test fails.
//!
//! See ../../STANDARD.md for the test contract.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use tokio::process::Command;

#[derive(Parser)]
#[command(name = "qa-runner", version, about)]
struct Cli {
    /// Root of the test tree (contains <owner>/ dirs).
    #[arg(long, default_value = "tests")]
    tests_dir: PathBuf,
    #[arg(long)]
    release: String,
    #[arg(long, default_value = "")]
    flavor: String,
    /// Built image file (enables image-scope tests).
    #[arg(long)]
    image: Option<PathBuf>,
    /// Live test node (enables cluster-scope tests).
    #[arg(long)]
    node_ip: Option<String>,
    #[arg(long)]
    node_name: Option<String>,
    /// SSH command prefix, e.g. "ssh -o StrictHostKeyChecking=no root@1.2.3.4".
    #[arg(long)]
    ssh: Option<String>,
    /// Kube API base URL on the node.
    #[arg(long, default_value = "http://127.0.0.1:6443")]
    api: String,
    /// Where tests drop logs/artifacts.
    #[arg(long, default_value = "/tmp/qa-artifacts")]
    artifacts: PathBuf,
    /// File GitHub issues for failures (needs gh auth).
    #[arg(long)]
    file_issues: bool,
    /// Write the JSON report here.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Default owner org for `glennswest/<dir>` mapping.
    #[arg(long, default_value = "glennswest")]
    org: String,
    /// On failure, run must-gather against the node into the artifacts dir.
    #[arg(long)]
    gather: bool,
    /// must-gather binary.
    #[arg(long, default_value = "must-gather")]
    must_gather_bin: String,
    /// gather/ collectors dir to pass to must-gather.
    #[arg(long)]
    collectors_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Meta {
    name: String,
    owner: Option<String>,
    desc: String,
    scope: Scope,
    blocking: bool,
    timeout: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Image,
    Cluster,
    Component,
}

struct Test {
    path: PathBuf,
    dir: String, // top dir under tests/ (owner default / "overall")
    meta: Meta,
}

#[derive(Serialize)]
struct TestResult {
    name: String,
    owner: String,
    scope: Scope,
    blocking: bool,
    passed: bool,
    duration_secs: u64,
    log: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<String>,
}

#[derive(Serialize)]
struct Report {
    release: String,
    flavor: String,
    total: usize,
    passed: usize,
    failed: usize,
    blocking_failures: usize,
    tombstone: bool,
    results: Vec<TestResult>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.artifacts)?;
    let tests = discover(&cli.tests_dir, &cli.org)?;

    let mut results = Vec::new();
    for t in &tests {
        // Scope gating: skip what this run can't exercise.
        match t.meta.scope {
            Scope::Image if cli.image.is_none() => continue,
            Scope::Cluster if cli.ssh.is_none() => continue,
            _ => {}
        }
        let owner = resolve_owner(t, &cli.org);
        let Some(owner) = owner else {
            eprintln!("skip {}: overall test without QA-Owner", t.meta.name);
            continue;
        };
        let (passed, log, secs) = run_test(t, &cli).await;
        let status = if passed { "PASS" } else { "FAIL" };
        println!("[{status}] {} ({owner})", t.meta.name);

        let mut issue = None;
        if !passed && cli.file_issues {
            issue = file_issue(t, &owner, &cli.release, &log).await;
        }
        results.push(TestResult {
            name: t.meta.name.clone(),
            owner,
            scope: t.meta.scope,
            blocking: t.meta.blocking,
            passed,
            duration_secs: secs,
            log: log.chars().take(4000).collect(),
            issue,
        });
    }

    let failed = results.iter().filter(|r| !r.passed).count();
    let blocking_failures = results
        .iter()
        .filter(|r| !r.passed && r.blocking)
        .count();

    // On any failure, capture debug data with must-gather (our must-gather).
    if failed > 0 && cli.gather && let Some(ip) = &cli.node_ip {
        {
            let out = cli
                .artifacts
                .join(format!("must-gather-{}", cli.release));
            let mut args = vec![
                "--nodes".to_string(),
                ip.clone(),
                "--out".to_string(),
                out.to_string_lossy().to_string(),
            ];
            if let Some(cd) = &cli.collectors_dir {
                args.push("--collectors-dir".into());
                args.push(cd.to_string_lossy().to_string());
            }
            println!("failures detected — running must-gather -> {}", out.display());
            let _ = Command::new(&cli.must_gather_bin).args(&args).status().await;
        }
    }
    let report = Report {
        release: cli.release.clone(),
        flavor: cli.flavor.clone(),
        total: results.len(),
        passed: results.iter().filter(|r| r.passed).count(),
        failed,
        blocking_failures,
        tombstone: blocking_failures > 0,
        results,
    };
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(p) = &cli.report {
        std::fs::write(p, &json)?;
    }
    println!(
        "\n{} tests, {} passed, {} failed ({} blocking){}",
        report.total,
        report.passed,
        report.failed,
        report.blocking_failures,
        if report.tombstone {
            " — TOMBSTONE"
        } else {
            ""
        }
    );
    // Exit non-zero = tombstone the release.
    std::process::exit(report.blocking_failures.min(125) as i32);
}

/// Find executable test files under tests/<dir>/, parse their metadata.
fn discover(root: &Path, _org: &str) -> anyhow::Result<Vec<Test>> {
    let mut out = Vec::new();
    for dent in std::fs::read_dir(root)? {
        let dir = dent?.path();
        if !dir.is_dir() {
            continue;
        }
        let dirname = dir.file_name().unwrap().to_string_lossy().to_string();
        for f in std::fs::read_dir(&dir)? {
            let p = f?.path();
            if !p.is_file() {
                continue;
            }
            let fname = p.file_name().unwrap().to_string_lossy().to_string();
            if fname.starts_with('.') || fname.ends_with(".qa.toml") || fname == "README.md" {
                continue;
            }
            if !is_executable(&p) {
                continue;
            }
            let meta = parse_meta(&p, &fname);
            out.push(Test {
                path: p,
                dir: dirname.clone(),
                meta,
            });
        }
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(out)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn parse_meta(path: &Path, fname: &str) -> Meta {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines().take(40) {
        let l = line.trim_start_matches(['#', '/', ' ', '\t']);
        if let Some(rest) = l.strip_prefix("QA-")
            && let Some((k, v)) = rest.split_once(':')
        {
            kv.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let scope = match kv.get("scope").map(|s| s.as_str()) {
        Some("image") => Scope::Image,
        Some("component") => Scope::Component,
        _ => Scope::Cluster,
    };
    Meta {
        name: kv
            .get("name")
            .cloned()
            .unwrap_or_else(|| fname.trim_end_matches(".sh").to_string()),
        owner: kv.get("owner").cloned(),
        desc: kv.get("desc").cloned().unwrap_or_default(),
        scope,
        blocking: kv.get("severity").map(|s| s != "warn").unwrap_or(true),
        timeout: kv.get("timeout").and_then(|s| s.parse().ok()).unwrap_or(300),
    }
}

fn resolve_owner(t: &Test, org: &str) -> Option<String> {
    if let Some(o) = &t.meta.owner {
        return Some(o.clone());
    }
    if t.dir == "overall" {
        return None; // must be explicit
    }
    Some(format!("{org}/{}", t.dir))
}

async fn run_test(t: &Test, cli: &Cli) -> (bool, String, u64) {
    let log_path = cli.artifacts.join(format!("{}.log", slug(&t.dir, &t.meta.name)));
    let file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return (false, format!("cannot create log: {e}"), 0),
    };
    let errf = file.try_clone().ok();
    let start = std::time::Instant::now();
    let mut cmd = Command::new(&t.path);
    cmd.env("QA_RELEASE_ID", &cli.release)
        .env("QA_FLAVOR", &cli.flavor)
        .env("QA_API", &cli.api)
        .env("QA_ARTIFACTS", &cli.artifacts)
        .stdout(Stdio::from(file));
    if let Some(e) = errf {
        cmd.stderr(Stdio::from(e));
    }
    if let Some(i) = &cli.image {
        cmd.env("QA_IMAGE", i);
    }
    if let Some(ip) = &cli.node_ip {
        cmd.env("QA_NODE_IP", ip);
    }
    if let Some(n) = &cli.node_name {
        cmd.env("QA_NODE_NAME", n);
    }
    if let Some(s) = &cli.ssh {
        cmd.env("QA_SSH", s);
    }
    let passed = match cmd.spawn() {
        Ok(mut child) => {
            match tokio::time::timeout(Duration::from_secs(t.meta.timeout), child.wait()).await {
                Ok(Ok(status)) => status.success(),
                Ok(Err(_)) => false,
                Err(_) => {
                    let _ = child.start_kill();
                    false
                }
            }
        }
        Err(e) => {
            let _ = std::fs::write(&log_path, format!("spawn failed: {e}"));
            false
        }
    };
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    (passed, log, start.elapsed().as_secs())
}

fn slug(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// File (or update) a GitHub issue in the owning repo, deduped by a marker.
async fn file_issue(t: &Test, owner: &str, release: &str, log: &str) -> Option<String> {
    let marker = format!("<!-- qa:{}/{} -->", t.dir, slug("", &t.meta.name));
    // Already an open issue with this marker?
    let existing = gh(&[
        "issue", "list", "--repo", owner, "--state", "open", "--search", &marker, "--json",
        "number", "--jq", ".[0].number",
    ])
    .await
    .unwrap_or_default();
    let body = format!(
        "Automated QA failure from stormcos-builder.\n\n\
         - **test**: {}\n- **checks**: {}\n- **owner**: {owner}\n- **release**: `{release}`\n\
         - **scope**: {:?}\n\n```\n{}\n```\n\n{marker}\n",
        t.meta.name,
        if t.meta.desc.is_empty() { "—" } else { &t.meta.desc },
        t.meta.scope,
        log.chars().take(6000).collect::<String>(),
    );
    let num = existing.trim();
    if !num.is_empty() {
        let _ = gh(&[
            "issue",
            "comment",
            num,
            "--repo",
            owner,
            "--body",
            &format!("Still failing on release `{release}`.\n\n```\n{}\n```", tail(log, 40)),
        ])
        .await;
        Some(format!("{owner}#{num} (updated)"))
    } else {
        let title = format!("QA failure: {}", t.meta.name);
        gh(&["issue", "create", "--repo", owner, "--title", &title, "--body", &body])
            .await
            .map(|url| url.trim().to_string())
    }
}

async fn gh(args: &[&str]) -> Option<String> {
    let out = Command::new("gh").args(args).output().await.ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        eprintln!("gh {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        None
    }
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
