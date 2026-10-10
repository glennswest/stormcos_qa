//! The one retry helper of stormcos_qa (#57). Every call that leaves the
//! process goes through it, so a flaky dependency is retried the same way
//! everywhere and shows up in the logs instead of as a test failure.
//!
//! - [`with_backoff`]: a call that either works or fails. The operation
//!   says per attempt whether a failure is worth another try
//!   ([`Step::Transient`]) or is a real answer ([`Step::Final`], returned at
//!   once). Retries back off exponentially with jitter, honour a server's
//!   `Retry-After`, and stop at the policy's attempt count or deadline,
//!   whichever comes first. Giving up returns an [`Infra`] error:
//!   infrastructure (timeout, unreachable, 5xx), not a test failure.
//! - [`probe`]: a reachability probe, whose "no answer" is itself a result.
//!   It is retried until something answers or the policy runs out; the last
//!   result comes back with the number of attempts it took.
//!
//! Logged on stderr (stdout is the test's JSON lines):
//! `retry: GET /api/v1/nodes: succeeded on attempt 3 after 41 s`, and
//! `retry: GET /api/v1/nodes: gave up after 5 attempts / 1 min 52 s: <last error>`.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub mod http;

/// How hard to try. The defaults are documented in docs/retries.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Attempts in all, the first included (1 = no retry).
    pub attempts: u32,
    /// The wait after the first failure; it doubles per attempt.
    pub first_delay: Duration,
    /// The longest wait between two attempts (a `Retry-After` may ask more).
    pub max_delay: Duration,
    /// The whole operation, attempts and waits together: no retry starts
    /// whose wait would end past it.
    pub deadline: Duration,
}

impl Policy {
    /// An API call (apiserver, stormblock): 5 attempts, 0.5 s doubling to
    /// 8 s, at most 2 min. Each attempt also has the client's own timeout.
    pub const API: Policy = Policy {
        attempts: 5,
        first_delay: Duration::from_millis(500),
        max_delay: Duration::from_secs(8),
        deadline: Duration::from_secs(120),
    };
    /// Opening a session (ssh connect + login): 4 attempts, 1 s doubling to
    /// 5 s, at most 90 s.
    pub const CONNECT: Policy = Policy {
        attempts: 4,
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(5),
        deadline: Duration::from_secs(90),
    };
    /// A reachability probe that got no answer: 3 attempts, 1 s then 2 s
    /// apart, at most 60 s.
    pub const PROBE: Policy = Policy {
        attempts: 3,
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(2),
        deadline: Duration::from_secs(60),
    };
}

/// What one failed attempt means.
#[derive(Debug)]
pub enum Step<E> {
    /// Worth another try (timeout, refused, reset, 5xx, 429). `after`: the
    /// server's `Retry-After`, if it gave one.
    Transient { why: String, after: Option<Duration> },
    /// A real answer (4xx, a validation error): returned at once.
    Final(E),
}

impl<E> Step<E> {
    pub fn transient(why: impl Into<String>) -> Self {
        Step::Transient { why: why.into(), after: None }
    }
}

/// Gave up on an infrastructure failure: callers and runners report it as
/// "could not run", never as the thing under test failing.
#[derive(Debug, Clone)]
pub struct Infra {
    pub what: String,
    pub attempts: u32,
    pub elapsed: Duration,
    pub last: String,
    /// A write that may have landed (not retried for that reason).
    pub unsure: bool,
}

impl fmt::Display for Infra {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.attempts;
        write!(f, "{}: gave up after {n} attempt{} / {}: {}", self.what, if n == 1 { "" } else { "s" }, human(self.elapsed), self.last)
    }
}

impl std::error::Error for Infra {}

impl Infra {
    /// A write that failed in a way that may have landed, so it is not
    /// retried blindly: still infrastructure, not a real answer.
    pub fn unsure(what: impl Into<String>, why: impl Into<String>, elapsed: Duration) -> Self {
        Infra { what: what.into(), attempts: 1, elapsed, last: why.into(), unsure: true }
    }
}

/// Whether this error (anywhere in its chain) is an [`Infra`].
pub fn is_infra(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Infra>())
}

/// Whether this error is an [`Infra`] for a write that may have landed.
pub fn is_unsure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Infra>().is_some_and(|i| i.unsure))
}

/// After a create that may have landed (check-then-act): the object found
/// by its name is ours when it carries every label we sent. With no labels
/// sent there is no telling, so it is not ours.
pub fn labels_match(sent: &serde_json::Value, found: &serde_json::Value) -> bool {
    match sent.as_object() {
        Some(m) if !m.is_empty() => m.iter().all(|(k, v)| found.get(k) == Some(v)),
        _ => false,
    }
}

/// Run `op` (given the 1-based attempt number) under `policy`.
pub async fn with_backoff<T, F, Fut>(policy: &Policy, what: &str, mut op: F) -> anyhow::Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, Step<anyhow::Error>>>,
{
    let t0 = Instant::now();
    let mut n = 0;
    loop {
        n += 1;
        match op(n).await {
            Ok(v) => {
                if n > 1 {
                    eprintln!("retry: {what}: succeeded on attempt {n} after {}", human(t0.elapsed()));
                }
                return Ok(v);
            }
            Err(Step::Final(e)) => return Err(e),
            Err(Step::Transient { why, after }) => {
                let wait = delay(policy, n, after);
                if n >= policy.attempts || t0.elapsed() + wait > policy.deadline {
                    let gave = Infra { what: what.into(), attempts: n, elapsed: t0.elapsed(), last: why, unsure: false };
                    eprintln!("retry: {gave}");
                    return Err(gave.into());
                }
                eprintln!("retry: {what}: attempt {n} failed ({why}); next in {}", human(wait));
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Probe until `op` reports an answer (`(result, true)`) or the policy runs
/// out; returns the last result and the attempts it took. Not answering is
/// a result here, so only an answer after a retry is logged.
pub async fn probe<T, F, Fut>(policy: &Policy, what: &str, mut op: F) -> (T, u32)
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = (T, bool)>,
{
    let t0 = Instant::now();
    let mut n = 0;
    loop {
        n += 1;
        let (v, answered) = op(n).await;
        if answered {
            if n > 1 {
                eprintln!("retry: {what}: answered on attempt {n} after {}", human(t0.elapsed()));
            }
            return (v, n);
        }
        let wait = delay(policy, n, None);
        if n >= policy.attempts || t0.elapsed() + wait > policy.deadline {
            return (v, n);
        }
        tokio::time::sleep(wait).await;
    }
}

/// The wait after failed attempt `n`: `first_delay · 2^(n-1)`, capped at
/// `max_delay`, then jittered into its upper half (so many callers that
/// failed together do not retry together). A `Retry-After` is a floor.
pub fn delay(policy: &Policy, n: u32, after: Option<Duration>) -> Duration {
    let base = policy.first_delay.saturating_mul(1u32 << (n.saturating_sub(1)).min(16)).min(policy.max_delay);
    let half = base / 2;
    let jittered = half + half.mul_f64(jitter());
    match after {
        Some(a) => jittered.max(a),
        None => jittered,
    }
}

/// A number in [0, 1): xorshift over a clock-seeded state. Spreading
/// retries needs no more than this.
fn jitter() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x9E37_79B9, |d| d.as_nanos() as u64) | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// `0.4 s`, `41 s`, `2 min 3 s`.
pub fn human(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1} s")
    } else if s < 60.0 {
        format!("{} s", s as u64)
    } else {
        format!("{} min {} s", s as u64 / 60, s as u64 % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const FAST: Policy = Policy {
        attempts: 4,
        first_delay: Duration::from_millis(2),
        max_delay: Duration::from_millis(8),
        deadline: Duration::from_secs(5),
    };

    /// An operation that fails `fails` times, then answers `ok`.
    async fn flaky(calls: &AtomicU32, fails: u32) -> Result<&'static str, Step<anyhow::Error>> {
        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= fails { Err(Step::transient(format!("timeout #{n}"))) } else { Ok("ok") }
    }

    #[tokio::test]
    async fn fails_n_times_then_succeeds() {
        let calls = AtomicU32::new(0);
        let v = with_backoff(&FAST, "fake", |_| flaky(&calls, 3)).await.unwrap();
        assert_eq!((v, calls.load(Ordering::SeqCst)), ("ok", 4));
    }

    #[tokio::test]
    async fn gives_up_after_the_attempts_as_infrastructure() {
        let calls = AtomicU32::new(0);
        let e = with_backoff(&FAST, "fake", |_| flaky(&calls, 10)).await.unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(is_infra(&e));
        let i = e.downcast_ref::<Infra>().unwrap();
        assert_eq!(i.attempts, 4);
        assert_eq!(i.last, "timeout #4");
        assert!(e.to_string().starts_with("fake: gave up after 4 attempts / "), "{e}");
        // Still recognised under context added by a caller.
        assert!(is_infra(&e.context("listing pods")));
    }

    #[tokio::test]
    async fn a_real_answer_is_not_retried() {
        let calls = AtomicU32::new(0);
        let e = with_backoff(&FAST, "fake", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(Step::Final(anyhow::anyhow!("403 forbidden")))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!is_infra(&e));
        assert_eq!(e.to_string(), "403 forbidden");
    }

    #[tokio::test]
    async fn the_deadline_caps_the_retries() {
        let p = Policy { attempts: 100, first_delay: Duration::from_millis(20), max_delay: Duration::from_millis(20), deadline: Duration::from_millis(60) };
        let calls = AtomicU32::new(0);
        let t = Instant::now();
        let e = with_backoff(&p, "fake", |_| flaky(&calls, 1000)).await.unwrap_err();
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        assert!(calls.load(Ordering::SeqCst) < 10);
        assert!(is_infra(&e));
    }

    #[tokio::test]
    async fn retry_after_is_honoured() {
        let calls = AtomicU32::new(0);
        let t = Instant::now();
        with_backoff(&FAST, "fake", |_| async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(Step::Transient { why: "429".into(), after: Some(Duration::from_millis(150)) })
            } else {
                Ok(())
            }
        })
        .await
        .unwrap();
        assert!(t.elapsed() >= Duration::from_millis(150));
    }

    #[tokio::test]
    async fn a_probe_stops_at_the_first_answer_and_counts() {
        let calls = AtomicU32::new(0);
        let (r, n) = probe(&FAST, "fake", |_| async {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 { ("timeout", false) } else { ("open", true) }
        })
        .await;
        assert_eq!((r, n), ("open", 3));
        let (r, n) = probe(&FAST, "fake", |_| async { ("timeout", false) }).await;
        assert_eq!((r, n), ("timeout", 4));
    }

    #[test]
    fn delays_double_jitter_and_cap() {
        for _ in 0..100 {
            let d1 = delay(&Policy::API, 1, None);
            assert!(d1 >= Duration::from_millis(250) && d1 <= Duration::from_millis(500), "{d1:?}");
            let d3 = delay(&Policy::API, 3, None);
            assert!(d3 >= Duration::from_secs(1) && d3 <= Duration::from_secs(2), "{d3:?}");
            let d9 = delay(&Policy::API, 9, None);
            assert!(d9 >= Duration::from_secs(4) && d9 <= Duration::from_secs(8), "{d9:?}");
        }
        assert_eq!(delay(&Policy::API, 1, Some(Duration::from_secs(30))), Duration::from_secs(30));
    }

    #[test]
    fn a_namesake_is_ours_only_with_our_labels() {
        let sent = serde_json::json!({"storm.io/test-run": "r1", "app": "x"});
        assert!(labels_match(&sent, &serde_json::json!({"storm.io/test-run": "r1", "app": "x", "extra": "y"})));
        assert!(!labels_match(&sent, &serde_json::json!({"storm.io/test-run": "r0", "app": "x"})));
        assert!(!labels_match(&serde_json::json!({}), &serde_json::json!({"a": "b"})));
        assert!(!labels_match(&serde_json::Value::Null, &serde_json::Value::Null));
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(human(Duration::from_millis(400)), "0.4 s");
        assert_eq!(human(Duration::from_secs(41)), "41 s");
        assert_eq!(human(Duration::from_secs(123)), "2 min 3 s");
    }
}
