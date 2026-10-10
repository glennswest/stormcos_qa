//! The cluster API, from a workstation (`--api https://<node>:6443`, a token
//! file, the cluster's CA or `--insecure`) or from inside a pod (the mounted
//! ServiceAccount). No plain-http default: rustkube serves TLS (#11).

use anyhow::{Context, Result, bail};
use retry::Policy;
use retry::http::{self, Repeat};
use serde_json::Value;

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Api {
    pub base: String,
    token_file: Option<String>,
    http: reqwest::Client,
}

/// An answer: status and body text (JSON or not).
pub struct Got {
    pub code: u16,
    pub text: String,
}

impl Got {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
}

impl Api {
    /// `api` empty: in-cluster (`KUBERNETES_SERVICE_HOST`, the mounted CA and
    /// token). `token_file` none: the ServiceAccount's, when there is one.
    pub fn new(api: &str, token_file: Option<&str>, ca_file: Option<&str>, insecure: bool) -> Result<Self> {
        let base = if api.is_empty() {
            let host = std::env::var("KUBERNETES_SERVICE_HOST")
                .context("no --api given and not in a pod (KUBERNETES_SERVICE_HOST unset): pass --api https://<node>:6443")?;
            let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
            format!("https://{host}:{port}")
        } else {
            api.trim_end_matches('/').to_string()
        };
        if base.starts_with("http://") {
            bail!("{base}: the apiserver serves TLS; use https:// (#11)");
        }
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60));
        if insecure {
            b = b.danger_accept_invalid_certs(true);
        } else {
            let ca = ca_file.map(str::to_string).or_else(|| {
                let f = format!("{SA}/ca.crt");
                std::path::Path::new(&f).exists().then_some(f)
            });
            if let Some(ca) = ca {
                let pem = std::fs::read(&ca).with_context(|| format!("reading the CA {ca}"))?;
                b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
            }
        }
        let token_file = token_file.map(str::to_string).or_else(|| {
            let f = format!("{SA}/token");
            std::path::Path::new(&f).exists().then_some(f)
        });
        Ok(Api { base, token_file, http: b.build()? })
    }

    pub fn token_file(&self) -> Option<&str> {
        self.token_file.as_deref()
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // Re-read every time: bound ServiceAccount tokens rotate in place.
        match self.token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
            Some(t) if !t.trim().is_empty() => req.bearer_auth(t.trim()),
            _ => req,
        }
    }

    /// One call through the retry helper (#57). GET and DELETE ride out
    /// timeouts and 5xx; other methods are repeated only when they
    /// certainly did not land. A transient failure that outlasts the policy
    /// is a `retry::Infra` error, any other status a `Got`.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Got> {
        let repeat = if matches!(method, reqwest::Method::GET | reqwest::Method::DELETE) { Repeat::Safe } else { Repeat::Unsafe };
        let url = format!("{}{path}", self.base);
        let a = http::send(&Policy::API, &format!("{method} {path}"), repeat, || {
            let mut req = self.http.request(method.clone(), &url);
            if let Some(b) = body {
                req = req.json(b);
            }
            self.auth(req)
        })
        .await?;
        Ok(Got { code: a.code, text: String::from_utf8_lossy(&a.body).into_owned() })
    }

    pub async fn get(&self, path: &str) -> Result<Got> {
        self.call(reqwest::Method::GET, path, None).await
    }

    /// Any method. A POST is a create: after a failure that may have
    /// landed, check-then-act (the object by its name, with the labels we
    /// sent, is ours; absent, it is sent again).
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Got> {
        if method != reqwest::Method::POST {
            return self.call(method, path, body).await;
        }
        let mut tries = 0;
        loop {
            tries += 1;
            let err = match self.call(reqwest::Method::POST, path, body).await {
                Err(e) if retry::is_unsure(&e) && tries < Policy::API.attempts => e,
                other => return other,
            };
            let sent = body.map(|b| &b["metadata"]).cloned().unwrap_or(Value::Null);
            let Some(name) = sent["name"].as_str() else { return Err(err) };
            // A namespace is cluster-scoped: its name is under /namespaces too.
            let got = self.get(&format!("{path}/{name}")).await?;
            let found: Value = serde_json::from_str(&got.text).unwrap_or(Value::Null);
            if got.ok() && retry::labels_match(&sent["labels"], &found["metadata"]["labels"]) {
                eprintln!("retry: POST {path}: {name} exists with our labels after an unsure create: done");
                return Ok(got);
            }
            if got.code != 404 {
                return Err(err);
            }
        }
    }

    /// Every item of a list, following `continue` (rustkube pages at 500).
    /// The result is one List object with all the items, as a reader expects.
    pub async fn list(&self, path: &str) -> Result<std::result::Result<Value, Got>> {
        let mut items: Vec<Value> = Vec::new();
        let mut first: Option<Value> = None;
        let mut cont = String::new();
        loop {
            let sep = if path.contains('?') { '&' } else { '?' };
            let page = if cont.is_empty() { format!("{path}{sep}limit=500") } else { format!("{path}{sep}limit=500&continue={cont}") };
            let g = self.get(&page).await?;
            if !g.ok() {
                return Ok(Err(g));
            }
            let mut v: Value = serde_json::from_str(&g.text).with_context(|| format!("{path}: not JSON"))?;
            if let Some(a) = v.get_mut("items").and_then(Value::as_array_mut) {
                items.append(a);
            }
            cont = v["metadata"]["continue"].as_str().unwrap_or("").to_string();
            if first.is_none() {
                first = Some(v);
            }
            if cont.is_empty() {
                break;
            }
        }
        let mut out = first.unwrap_or_else(|| serde_json::json!({}));
        out["items"] = Value::Array(items);
        if let Some(m) = out.get_mut("metadata").and_then(Value::as_object_mut) {
            m.remove("continue");
        }
        Ok(Ok(out))
    }
}

pub fn items(v: &Value) -> &[Value] {
    v["items"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// A path segment for a query string.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
