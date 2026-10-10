//! HTTP through [`with_backoff`](crate::with_backoff): which failures are
//! worth another try, and which requests may be repeated.
//!
//! - Transient: a transport error (refused, reset, timeout) and the
//!   statuses 408, 429, 500, 502, 503 and 504, honouring `Retry-After`.
//! - Real answers: every other status (a 404, 409 or 403 is the server's
//!   answer, returned to the caller at once).
//! - A [`Repeat::Safe`] request (GET, DELETE, a read-only watch) is retried
//!   on any transient failure. A [`Repeat::Unsafe`] one (a POST, a
//!   subresource PUT such as a VM restart) only when the server certainly
//!   did not act: the connection never opened, or it answered 408, 429 or
//!   503. Anything else (a timeout or reset after sending, a 500, 502 or
//!   504) may have landed: it fails at once with an [`Infra`] marked
//!   `unsure`, and the caller decides (a create checks whether its object
//!   exists, check-then-act).

use std::time::{Duration, Instant};

use crate::{Infra, Policy, Step, with_backoff};

/// Whether a request may be sent twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    /// Idempotent: GET, DELETE, a watch.
    Safe,
    /// Not idempotent: retried only when it certainly did not land.
    Unsafe,
}

/// A status worth another try.
pub fn transient_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

/// A status that says the server did not act on the request.
pub fn not_processed(code: u16) -> bool {
    matches!(code, 408 | 429 | 503)
}

/// `Retry-After` in seconds (the HTTP-date form is not used by our servers).
pub fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    h.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A reqwest error with its causes ("error sending request" alone says nothing).
pub fn describe(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

/// An answer: status and the whole body.
pub struct Answer {
    pub code: u16,
    pub body: Vec<u8>,
}

/// Send the request `build` makes (once per attempt) and read its body,
/// retrying under `policy` as [`Repeat`] allows. `Ok` is any non-transient
/// status, for the caller to read; transient ones that outlast the policy,
/// and transport errors, are an [`Infra`] error.
pub async fn send<F>(policy: &Policy, what: &str, repeat: Repeat, build: F) -> anyhow::Result<Answer>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let t0 = Instant::now();
    with_backoff(policy, what, |_| async {
        let r = attempt(what, repeat, &build, t0).await?;
        let code = r.status().as_u16();
        match r.bytes().await {
            Ok(b) => Ok(Answer { code, body: b.to_vec() }),
            Err(e) => Err(failed(what, repeat, false, format!("reading the answer: {}", describe(&e)), t0)),
        }
    })
    .await
}

/// As [`send`], but the response itself, unread (a watch reads it as a
/// stream). Only for [`Repeat::Safe`] requests.
pub async fn open<F>(policy: &Policy, what: &str, build: F) -> anyhow::Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let t0 = Instant::now();
    with_backoff(policy, what, |_| attempt(what, Repeat::Safe, &build, t0)).await
}

async fn attempt<F>(what: &str, repeat: Repeat, build: &F, t0: Instant) -> Result<reqwest::Response, Step<anyhow::Error>>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let r = match build().send().await {
        Ok(r) => r,
        Err(e) => return Err(failed(what, repeat, e.is_connect(), describe(&e), t0)),
    };
    let code = r.status().as_u16();
    if !transient_status(code) {
        return Ok(r);
    }
    let after = retry_after(r.headers());
    let text = r.text().await.unwrap_or_default();
    let why = format!("answered {code}: {}", text.chars().take(200).collect::<String>().trim());
    if repeat == Repeat::Safe || not_processed(code) {
        Err(Step::Transient { why, after })
    } else {
        Err(Step::Final(Infra::unsure(what, format!("{why} (not retried: the request may have been acted on)"), t0.elapsed()).into()))
    }
}

/// A failed attempt: transient when it may be repeated, else an unsure Infra.
fn failed(what: &str, repeat: Repeat, not_sent: bool, why: String, t0: Instant) -> Step<anyhow::Error> {
    if repeat == Repeat::Safe || not_sent {
        Step::transient(why)
    } else {
        Step::Final(Infra::unsure(what, format!("{why} (not retried: the request may have been acted on)"), t0.elapsed()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FAST: Policy = Policy {
        attempts: 4,
        first_delay: Duration::from_millis(2),
        max_delay: Duration::from_millis(8),
        deadline: Duration::from_secs(5),
    };

    /// A server that answers `codes` in turn (the last one from then on),
    /// counting requests.
    async fn server(codes: Vec<u16>) -> (String, Arc<AtomicU32>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let n = h.fetch_add(1, Ordering::SeqCst) as usize;
                let code = codes[n.min(codes.len() - 1)];
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let body = format!("{{\"code\":{code}}}");
                    let extra = if code == 429 { "Retry-After: 0\r\n" } else { "" };
                    let resp = format!("HTTP/1.1 {code} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn a_get_rides_out_5xx() {
        let (url, hits) = server(vec![503, 500, 200]).await;
        let c = reqwest::Client::new();
        let a = send(&FAST, "GET /x", Repeat::Safe, || c.get(&url)).await.unwrap();
        assert_eq!((a.code, hits.load(Ordering::SeqCst)), (200, 3));
    }

    #[tokio::test]
    async fn a_real_answer_comes_back_at_once() {
        let (url, hits) = server(vec![404]).await;
        let c = reqwest::Client::new();
        let a = send(&FAST, "GET /x", Repeat::Safe, || c.get(&url)).await.unwrap();
        assert_eq!((a.code, hits.load(Ordering::SeqCst)), (404, 1));
    }

    #[tokio::test]
    async fn a_post_repeats_only_what_did_not_land() {
        let (url, hits) = server(vec![429, 503, 201]).await;
        let c = reqwest::Client::new();
        let a = send(&FAST, "POST /x", Repeat::Unsafe, || c.post(&url)).await.unwrap();
        assert_eq!((a.code, hits.load(Ordering::SeqCst)), (201, 3));

        let (url, hits) = server(vec![500, 201]).await;
        let e = send(&FAST, "POST /x", Repeat::Unsafe, || c.post(&url)).await.err().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a 500 may have created it");
        assert!(crate::is_unsure(&e) && crate::is_infra(&e), "{e}");
    }

    #[tokio::test]
    async fn nothing_listening_is_infrastructure() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let c = reqwest::Client::new();
        // Refused means not sent: even a POST is retried, then given up on.
        let e = send(&FAST, "POST /x", Repeat::Unsafe, || c.post(&url)).await.err().unwrap();
        let i = e.downcast_ref::<Infra>().unwrap();
        assert_eq!(i.attempts, 4);
        assert!(!i.unsure);
    }
}
