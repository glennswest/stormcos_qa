//! The apiserver, from inside the Job's pod (service-account token, re-read
//! on every request because bound tokens rotate in place) or from outside
//! with `--api` and a token file.

use anyhow::{Context, Result};
use retry::Policy;
use retry::http::{self, Repeat};
use serde_json::Value;

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Client {
    base: String,
    token_file: String,
    http: reqwest::Client,
}

pub struct Resp {
    pub code: u16,
    pub body: Value,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
}

impl Client {
    /// `api` empty means in-cluster: `KUBERNETES_SERVICE_HOST`/`_PORT` and
    /// the mounted CA.
    pub async fn new(api: &str, token_file: Option<&str>, insecure: bool) -> Result<Self> {
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60));
        let base = if api.is_empty() {
            let host = std::env::var("KUBERNETES_SERVICE_HOST")
                .context("no --api / STORM_API and KUBERNETES_SERVICE_HOST is unset (not in a pod)")?;
            let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
            format!("https://{host}:{port}")
        } else {
            api.trim_end_matches('/').to_string()
        };
        if insecure {
            b = b.danger_accept_invalid_certs(true);
        } else if let Ok(ca) = tokio::fs::read(format!("{SA}/ca.crt")).await {
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&ca)?);
        }
        Ok(Client {
            base,
            token_file: token_file.map(str::to_string).unwrap_or_else(|| format!("{SA}/token")),
            http: b.build()?,
        })
    }

    /// One call through the retry helper (#57): `Repeat::Safe` calls ride
    /// out timeouts and 5xx; an unsafe one is repeated only when it
    /// certainly did not land. A transient failure that outlasts the policy
    /// is a `retry::Infra` error ("could not run"), any other status a `Resp`.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>, repeat: Repeat) -> Result<Resp> {
        let a = self.raw(method, path, body, repeat).await?;
        let text = String::from_utf8_lossy(&a.body).into_owned();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Resp { code: a.code, body })
    }

    async fn raw(&self, method: reqwest::Method, path: &str, body: Option<&Value>, repeat: Repeat) -> Result<http::Answer> {
        let token = tokio::fs::read_to_string(&self.token_file).await.unwrap_or_default();
        let token = token.trim();
        let url = format!("{}{path}", self.base);
        http::send(&Policy::API, &format!("{method} {path}"), repeat, || {
            let mut r = self.http.request(method.clone(), &url);
            if let Some(b) = body {
                r = r.json(b);
            }
            if token.is_empty() { r } else { r.bearer_auth(token) }
        })
        .await
    }

    pub async fn get(&self, path: &str) -> Result<Resp> {
        self.call(reqwest::Method::GET, path, None, Repeat::Safe).await
    }

    /// Create an object. Repeated when it certainly did not land; after a
    /// failure that may have landed, check-then-act: the object by its name
    /// carrying the labels we sent is ours, and the create is done; absent,
    /// it is sent again.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Resp> {
        let mut tries = 0;
        loop {
            tries += 1;
            let err = match self.post_once(path, body).await {
                Err(e) if retry::is_unsure(&e) && tries < Policy::API.attempts => e,
                other => return other,
            };
            let Some(name) = body["metadata"]["name"].as_str() else { return Err(err) };
            let got = self.get(&format!("{path}/{name}")).await?;
            if got.ok() && retry::labels_match(&body["metadata"]["labels"], &got.body["metadata"]["labels"]) {
                eprintln!("retry: POST {path}: {name} exists with our labels after an unsure create: done");
                return Ok(got);
            }
            if got.code != 404 {
                return Err(err);
            }
        }
    }

    /// One create, no check-then-act: a load test that measures its own
    /// creates and retries a whole attempt (turbomode, #26).
    pub async fn post_once(&self, path: &str, body: &Value) -> Result<Resp> {
        self.call(reqwest::Method::POST, path, Some(body), Repeat::Unsafe).await
    }

    /// A subresource action (a VM restart): not idempotent, so repeated
    /// only when it certainly did not land.
    pub async fn put(&self, path: &str) -> Result<Resp> {
        self.call(reqwest::Method::PUT, path, Some(&serde_json::json!({})), Repeat::Unsafe).await
    }

    /// DELETE is idempotent: a repeat finds it gone (404) or going.
    pub async fn delete(&self, path: &str) -> Result<Resp> {
        self.call(reqwest::Method::DELETE, path, None, Repeat::Safe).await
    }

    /// DELETE only the object with this UID (a precondition): a namesake
    /// made since is left alone.
    pub async fn delete_uid(&self, path: &str, uid: &str) -> Result<Resp> {
        let opts = serde_json::json!({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid}});
        self.call(reqwest::Method::DELETE, path, Some(&opts), Repeat::Safe).await
    }

    /// GET as text, whatever it is (a pod log whose one line is JSON stays text).
    pub async fn text(&self, path: &str) -> Result<(u16, String)> {
        let a = self.raw(reqwest::Method::GET, path, None, Repeat::Safe).await?;
        Ok((a.code, String::from_utf8_lossy(&a.body).into_owned()))
    }

    /// GET a watch: the response, to read its JSON lines chunk by chunk.
    /// Opening it is retried; a watch that ends is the caller's to resume.
    pub async fn watch(&self, path: &str) -> Result<reqwest::Response> {
        let token = tokio::fs::read_to_string(&self.token_file).await.unwrap_or_default();
        let token = token.trim();
        let url = format!("{}{path}", self.base);
        let r = http::open(&Policy::API, &format!("WATCH {path}"), || {
            let r = self.http.get(&url);
            if token.is_empty() { r } else { r.bearer_auth(token) }
        })
        .await?;
        anyhow::ensure!(r.status().is_success(), "watch answered {}", r.status());
        Ok(r)
    }
}

/// One look of a wait loop: the answer, or none, remembering a failed look
/// (an apiserver the retry helper gave up on) until the next good one.
pub fn looked(got: Result<Resp>, failed: &mut Option<anyhow::Error>) -> Option<Resp> {
    match got {
        Ok(r) => {
            *failed = None;
            Some(r)
        }
        Err(e) => {
            *failed = Some(e);
            None
        }
    }
}

/// A wait that ran out: when its last look failed, that failure (so an
/// unreachable apiserver reads "could not run", #57), else `why`.
pub fn timed_out(failed: Option<anyhow::Error>, why: String) -> anyhow::Error {
    match failed {
        Some(e) => e.context(format!("{why}; the last look failed")),
        None => anyhow::anyhow!(why),
    }
}

pub fn vms(ns: &str) -> String {
    format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachines")
}

pub fn vmis(ns: &str) -> String {
    format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstances")
}

pub fn subresource(ns: &str, vm: &str, verb: &str) -> String {
    format!("/apis/subresources.kubevirt.io/v1/namespaces/{ns}/virtualmachines/{vm}/{verb}")
}

/// This test's own image: the runner's Job pod carries `storm.io/test-run`
/// and `storm.io/suite` (a suite's helper pods carry only the first), else
/// the pod named `$HOSTNAME` (#54: under rustkube-node that name may not be
/// the pod's).
pub async fn own_image(kube: &Client, ns: &str, run: &str) -> Option<String> {
    let image = |p: &Value| p["spec"]["containers"][0]["image"].as_str().map(str::to_string);
    if let Ok(r) = kube.get(&format!("/api/v1/namespaces/{ns}/pods?labelSelector=storm.io%2Ftest-run%3D{run},storm.io%2Fsuite")).await
        && let Some(i) = items(&r.body).first().and_then(image)
    {
        return Some(i);
    }
    let host = std::env::var("HOSTNAME").unwrap_or_default();
    let r = kube.get(&format!("/api/v1/namespaces/{ns}/pods/{host}")).await.ok()?;
    if r.ok() { image(&r.body) } else { None }
}

/// `items` of a list response, empty on anything else.
pub fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}

/// A Kubernetes quantity (`2Gi`, `16318440Ki`, `1G`, plain bytes) in bytes.
pub fn quantity_bytes(q: &str) -> Option<u64> {
    let q = q.trim();
    let split = q.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(q.len());
    let (n, unit) = q.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mul: f64 = match unit {
        "" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0_f64.powi(4),
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        _ => return None,
    };
    Some((n * mul) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(quantity_bytes("2Gi"), Some(2 << 30));
        assert_eq!(quantity_bytes("16Ki"), Some(16 << 10));
        assert_eq!(quantity_bytes("1G"), Some(1_000_000_000));
        assert_eq!(quantity_bytes("123"), Some(123));
        assert_eq!(quantity_bytes("5Xi"), None);
    }
}
