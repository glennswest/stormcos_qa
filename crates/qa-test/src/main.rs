//! stormcos_qa's test container, per stormcentral docs/test-standard.md:
//! one image, started as `/test <suite>`.
//!
//! - `short`  — the prerequisites the other suites stand on (`short.rs`);
//! - `medium` — namespace isolation: VMs and pods in an isolated namespace
//!   talk to each other and nothing else (#18, `medium.rs`);
//! - `long`   — the overnight soak: container waves (#17, `containers.rs`)
//!   and VM waves (#16, `wave.rs`), alternating (`long.rs`);
//! - `turbomode` — the explicit load test: 1,000 sleeping Pods, then 100
//!   Pods with a SQLite PVC each, with a read-only storage audit on the node
//!   (#26, `turbomode.rs`, `turbo_audit.rs`). Never part of short|medium|long.
//!
//! and helpers the suites start as pods from this same image, so a run
//! fetches nothing from outside the machine:
//!
//! - `serve`  — a TCP listener, a target to be reached (or not);
//! - `agent`  — probes a plan from inside a namespace and prints the results;
//! - `claim`  — the workload of `long`'s container waves: writes or verifies
//!   its claim, serves, exits once (#17, `claim.rs`);
//! - `sleep`, `sqlite` — turbomode's workloads (#26, `sqlite.rs`).
//!
//! Exit 0 all passed (or skipped), 1 something failed, 2 could not run.

mod agent;
mod census;
mod claim;
mod containers;
mod kube;
mod long;
mod medium;
mod rdp;
mod report;
mod short;
mod sqlite;
mod ssh;
mod turbo_audit;
mod turbomode;
#[cfg(test)]
mod turbomode_fake;
mod wave;

use clap::Parser;

/// Stack for the runtime's threads and for the thread that drives a suite.
/// musl's defaults are small, and a stack overflow in a static musl binary
/// is a bare SIGSEGV (exit 139) with no message (#26: turbomode on pvetest1).
const STACK: usize = 16 << 20;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(STACK)
        .build()
        .expect("tokio runtime");
    // The suite's future is boxed, so its size never lands on a stack.
    let code = rt.block_on(Box::pin(dispatch()));
    std::process::exit(code);
}

async fn dispatch() -> i32 {
    let mut argv: Vec<String> = std::env::args().collect();
    // `/test <mode> [flags]`; no mode: the runner's STORM_SUITE.
    let mode = if argv.len() > 1 && !argv[1].starts_with('-') {
        argv.remove(1)
    } else {
        std::env::var("STORM_SUITE").unwrap_or_default()
    };
    let code = match mode.as_str() {
        "short" => short::main(short::Args::parse_from(&argv)).await,
        "medium" => medium::main(medium::Args::parse_from(&argv)).await,
        "long" => long::main(long::Args::parse_from(&argv)).await,
        "serve" => agent::serve(agent::ServeArgs::parse_from(&argv)).await,
        "agent" => agent::agent().await,
        "claim" => claim::main(claim::Args::parse_from(&argv)).await,
        "turbomode" => Box::pin(turbomode::main(turbomode::Args::parse_from(&argv))).await,
        "sleep" => sqlite::sleep(sqlite::SleepArgs::parse_from(&argv)).await,
        "sqlite" => sqlite::main(sqlite::Args::parse_from(&argv)).await,
        other => {
            eprintln!("usage: /test short|medium|long|turbomode|serve|agent|claim|sleep|sqlite [flags] (got {other:?}); --help per mode");
            2
        }
    };
    code
}
