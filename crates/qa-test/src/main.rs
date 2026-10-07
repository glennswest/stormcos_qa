//! stormcos_qa's test container, per stormcentral docs/test-standard.md:
//! one image, started as `/test <suite>`.
//!
//! - `short`  — the prerequisites the other suites stand on (`short.rs`);
//! - `medium` — namespace isolation: VMs and pods in an isolated namespace
//!   talk to each other and nothing else (#18, `medium.rs`);
//! - `long`   — the overnight soak: container waves (#17, `containers.rs`)
//!   and VM waves (#16, `wave.rs`), alternating (`long.rs`);
//!   `container-waves` is the container waves alone, sized for a day run
//!   (3 waves, 10–20 pods, budget 900 s), and `vm-waves` the VM waves alone
//!   (2 waves, 2 then up to 10 VMs, budget 1800 s);
//! - `turbomode` — the explicit load test: 100 sleeping Pods, then 25 Pods
//!   with a SQLite PVC each, with a read-only storage audit on the node
//!   (#26, `turbomode.rs`, `turbo_audit.rs`), in 15 min by day;
//!   `turbomode-night` is the full scale, 1,000 and 100 (#43). Never part
//!   of short|medium|long.
//!
//! and helpers the suites start as pods from this same image, so a run
//! fetches nothing from outside the machine:
//!
//! - `serve`  — a TCP listener, a target to be reached (or not);
//! - `agent`  — probes a plan from inside a namespace and prints the results;
//! - `claim`  — the workload of `long`'s container waves: writes or verifies
//!   its claim, serves, exits once (#17, `claim.rs`);
//! - `sleep`, `sqlite` — turbomode's workloads (#26, `sqlite.rs`);
//! - `crash`  — faults on purpose, to check the crash report (`crash.rs`).
//!
//! Exit 0 all passed (or skipped), 1 something failed, 2 could not run.

mod agent;
mod census;
mod claim;
mod containers;
mod crash;
mod kube;
mod long;
mod medium;
mod mustgather;
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
/// The suite's own thread: everything a suite does outside `tokio::spawn`
/// runs here, not on the process's main thread, whose stack is whatever the
/// container runtime inherited and has no guard page Rust can report.
const SUITE_STACK: usize = 64 << 20;

fn main() {
    crash::install();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(STACK)
        .build()
        .expect("tokio runtime");
    let suite = std::thread::Builder::new()
        .name("suite".into())
        .stack_size(SUITE_STACK)
        // The suite's future is boxed, so its size never lands on a stack.
        .spawn(move || rt.block_on(Box::pin(dispatch())))
        .expect("suite thread");
    let code = suite.join().unwrap_or_else(|_| {
        eprintln!("test: the suite panicked");
        2
    });
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
        "long" | "container-waves" | "vm-waves" => {
            let argv = long::argv(argv, &mode);
            long::main(long::Args::parse_from(&argv)).await
        }
        "must-gather" => mustgather::main(mustgather::Args::parse_from(&argv)).await,
        "serve" => agent::serve(agent::ServeArgs::parse_from(&argv)).await,
        "agent" => agent::agent().await,
        "claim" => claim::main(claim::Args::parse_from(&argv)).await,
        "turbomode" | "turbomode-night" => {
            let argv = turbomode::argv(argv, mode == "turbomode-night");
            Box::pin(turbomode::main(turbomode::Args::parse_from(&argv))).await
        }
        "sleep" => sqlite::sleep(sqlite::SleepArgs::parse_from(&argv)).await,
        "sqlite" => sqlite::main(sqlite::Args::parse_from(&argv)).await,
        // Faults on purpose: proves crash.rs's report in the built binary.
        "crash" => crash::fault(),
        other => {
            eprintln!("usage: /test short|medium|long|container-waves|vm-waves|turbomode[-night]|must-gather|serve|agent|claim|sleep|sqlite|crash [flags] (got {other:?}); --help per mode");
            2
        }
    };
    code
}
