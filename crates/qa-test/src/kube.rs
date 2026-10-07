//! The apiserver, from inside the Job's pod (service-account token, re-read
//! on every request because bound tokens rotate in place) or from outside
//! with `--api` and a token file.

use anyhow::{Context, Result};
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

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Resp> {
        let token = tokio::fs::read_to_string(&self.token_file).await.unwrap_or_default();
        let req = if token.trim().is_empty() { req } else { req.bearer_auth(token.trim()) };
        let r = req.send().await?;
        let code = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Resp { code, body })
    }

    pub async fn get(&self, path: &str) -> Result<Resp> {
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Resp> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body)).await
    }

    pub async fn put(&self, path: &str) -> Result<Resp> {
        self.send(self.http.put(format!("{}{path}", self.base)).json(&serde_json::json!({}))).await
    }

    pub async fn delete(&self, path: &str) -> Result<Resp> {
        self.send(self.http.delete(format!("{}{path}", self.base))).await
    }

    /// DELETE only the object with this UID (a precondition): a namesake
    /// made since is left alone.
    pub async fn delete_uid(&self, path: &str, uid: &str) -> Result<Resp> {
        let opts = serde_json::json!({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid}});
        self.send(self.http.delete(format!("{}{path}", self.base)).json(&opts)).await
    }

    /// GET as text, whatever it is (a pod log whose one line is JSON stays text).
    pub async fn text(&self, path: &str) -> Result<(u16, String)> {
        let token = tokio::fs::read_to_string(&self.token_file).await.unwrap_or_default();
        let req = self.http.get(format!("{}{path}", self.base));
        let req = if token.trim().is_empty() { req } else { req.bearer_auth(token.trim()) };
        let r = req.send().await?;
        let code = r.status().as_u16();
        Ok((code, r.text().await?))
    }

    /// GET a watch: the response, to read its JSON lines chunk by chunk.
    pub async fn watch(&self, path: &str) -> Result<reqwest::Response> {
        let token = tokio::fs::read_to_string(&self.token_file).await.unwrap_or_default();
        let req = self.http.get(format!("{}{path}", self.base));
        let req = if token.trim().is_empty() { req } else { req.bearer_auth(token.trim()) };
        let r = req.send().await?;
        anyhow::ensure!(r.status().is_success(), "watch answered {}", r.status());
        Ok(r)
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
