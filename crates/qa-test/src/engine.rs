//! The stormblock engine's management API as this test's client (#81,
//! stormcos#374): :9090 is moving to TLS with a certificate from the node CA.
//!
//! An `https://` URL is checked against one CA file and nothing else: no
//! built-in roots, no way to skip the check. An `http://` URL stays plain
//! until the engine's `required` switch.
//!
//! stormcos moves its callers' URL and CA in the registry or their unit, and
//! this test has neither: the runner gives it a node, not an engine URL. So
//! with no URL given it asks the node, once per run, as stormcentral does
//! (stormcentral#1173), and `required` needs nothing from here. It asks the
//! open `/api/v1/health` with no token. An engine that shows a certificate
//! the CA refuses stays https, so every call then fails naming the
//! certificate: a refusal never falls back to plain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use retry::http as rhttp;
use retry::{Policy, Step};

const PORT: u16 = 9090;
const HEALTH: &str = "/api/v1/health";
/// One attempt of asking the node; the engine is local or one hop away.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The engine as this run reaches it.
#[derive(Debug)]
pub struct Engine {
    /// `http(s)://<host>:<port>`, no trailing slash.
    pub base: String,
    /// Sends the token when there is one; trusts only the CA on https.
    pub http: reqwest::Client,
}

/// What the node's engine offers on its port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offered {
    /// TLS, whether its certificate verified or was refused.
    Tls,
    Plain,
    /// Neither answered: the engine is down or unreachable right now.
    Nothing,
}

/// The node CA when none is given: the host's tier-0 file the runner mounts
/// (`requires.toml`), else where a unit binds tier-0.
fn default_ca() -> Option<PathBuf> {
    let host = std::env::var("STORM_HOST_ROOT").ok().filter(|h| !h.trim().is_empty());
    host.map(|h| Path::new(&h).join("data/stormcert/ca.crt"))
        .into_iter()
        .chain([PathBuf::from("/etc/stormcert/ca.crt")])
        .find(|p| p.is_file())
}

fn read_ca(path: &Path) -> Result<Vec<u8>> {
    let pem = std::fs::read(path).with_context(|| format!("reading the engine's CA {}", path.display()))?;
    if !pem.windows(27).any(|w| w == b"-----BEGIN CERTIFICATE-----") {
        bail!("the engine's CA {} holds no PEM certificate", path.display());
    }
    Ok(pem)
}

/// `ca`: the only certificate trusted on https. Without one the client is
/// for plain http only: with no roots it can verify no certificate.
fn client(ca: Option<&[u8]>, token: Option<&str>, timeout: Duration) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(t) = token {
        let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))?;
        v.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    let mut b = reqwest::Client::builder().timeout(timeout).default_headers(headers).tls_built_in_root_certs(false);
    if let Some(pem) = ca {
        b = b.add_root_certificate(reqwest::Certificate::from_pem(pem).context("the engine's CA is not a PEM certificate")?);
    }
    Ok(b.build()?)
}

/// Ask the engine at `host:port` what it offers. TLS first, so an engine
/// answering both (`optional`) is reached over TLS.
async fn offered(at: &str, ca: &[u8], policy: &Policy) -> Result<Offered> {
    let tls = client(Some(ca), None, PROBE_TIMEOUT)?;
    let plain = client(None, None, PROBE_TIMEOUT)?;
    let what = format!("stormblock transport at {at}");
    let asked = retry::with_backoff(policy, &what, |_| async {
        let why = match tls.get(format!("https://{at}{HEALTH}")).send().await {
            Ok(_) => return Ok(Offered::Tls),
            Err(e) => rhttp::describe(&e),
        };
        if rhttp::certificate_refused(&why) {
            eprintln!("engine: {at} offers TLS, certificate refused: {why}");
            return Ok(Offered::Tls);
        }
        match plain.get(format!("http://{at}{HEALTH}")).send().await {
            Ok(r) if r.status().is_success() => Ok(Offered::Plain),
            Ok(r) => Err(Step::transient(format!("https: {why}; http answered {}", r.status().as_u16()))),
            Err(e) => Err(Step::transient(format!("https: {why}; http: {}", rhttp::describe(&e)))),
        }
    })
    .await;
    Ok(asked.unwrap_or(Offered::Nothing))
}

/// The run's engine. `url`: `--stormblock-url`; none: `<node>:9090`, by what
/// it offers. `ca`: `--stormblock-ca`; none: the node CA where it is found.
/// `Err`: an https engine with no usable CA, or a given CA that is unusable.
pub async fn resolve(url: Option<&str>, node: &str, ca: Option<&Path>, token: Option<&str>, timeout: Duration) -> Result<Engine> {
    let ca = ca.map(Path::to_path_buf).or_else(default_ca);
    resolve_at(url, &format!("{node}:{PORT}"), ca.as_deref(), token, timeout, &Policy::PROBE).await
}

async fn resolve_at(url: Option<&str>, at: &str, ca: Option<&Path>, token: Option<&str>, timeout: Duration, policy: &Policy) -> Result<Engine> {
    let (base, pem) = match url {
        Some(u) if u.starts_with("https://") => {
            let path = ca.with_context(|| format!("{u}: an https engine needs a CA: --stormblock-ca, STORMBLOCK_CA_FILE or <host>/data/stormcert/ca.crt"))?;
            (u.to_string(), Some(read_ca(path)?))
        }
        Some(u) if u.starts_with("http://") => (u.to_string(), None),
        Some(u) => bail!("--stormblock-url {u}: not an http or https URL"),
        None => match ca {
            None => (format!("http://{at}"), None),
            Some(path) => {
                let pem = read_ca(path)?;
                match offered(at, &pem, policy).await? {
                    Offered::Tls => (format!("https://{at}"), Some(pem)),
                    Offered::Plain | Offered::Nothing => (format!("http://{at}"), None),
                }
            }
        },
    };
    let http = client(pem.as_deref(), token, timeout)?;
    let base = base.trim_end_matches('/').to_string();
    eprintln!("engine: {base}");
    Ok(Engine { base, http })
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);
    /// Asking a stand-in needs no second try.
    const ONCE: Policy = Policy::PROBE.once();
    const GOLDEN: &str = "fedora-44-x86_64";

    /// What the stand-in's port speaks: `required`, `optional`, or the plain
    /// engine of today.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Tls,
        Both,
        Plain,
    }

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("qa-engine-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn openssl(d: &Path, args: &[&str]) {
        let o = Command::new("openssl").args(args).current_dir(d).output().expect("openssl runs");
        assert!(o.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    /// A CA named `who`, at `<d>/<who>.crt`, with a serving pair for 127.0.0.1.
    fn make_ca(d: &Path, who: &str) -> PathBuf {
        let ec = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes"];
        let (crt, key, srv_key, srv_csr, srv_crt) =
            (format!("{who}.crt"), format!("{who}.key"), format!("{who}-srv.key"), format!("{who}-srv.csr"), format!("{who}-srv.crt"));
        let mut a = vec!["req", "-x509"];
        a.extend(ec);
        a.extend(["-keyout", &key, "-out", &crt, "-days", "2", "-subj", "/CN=test-ca", "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign"]);
        openssl(d, &a);
        let mut a = vec!["req"];
        a.extend(ec);
        a.extend(["-keyout", &srv_key, "-out", &srv_csr, "-subj", "/CN=engine"]);
        openssl(d, &a);
        std::fs::write(d.join("san.ext"), "subjectAltName=IP:127.0.0.1\n").unwrap();
        openssl(d, &["x509", "-req", "-in", &srv_csr, "-CA", &crt, "-CAkey", &key, "-CAcreateserial", "-out", &srv_crt, "-days", "2", "-extfile", "san.ext"]);
        d.join(crt)
    }

    /// Answers every request with one body holding a volume list and health;
    /// `seen` gets `<tls|plain> <path> <authorization>`.
    async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, how: &str, seen: &Mutex<Vec<String>>) {
        let mut req = Vec::new();
        let mut buf = [0u8; 2048];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut buf).await {
                Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                _ => return,
            }
        }
        let text = String::from_utf8_lossy(&req).to_string();
        let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
        let auth = text.lines().find_map(|l| l.to_ascii_lowercase().starts_with("authorization:").then(|| l[14..].trim().to_string())).unwrap_or_default();
        seen.lock().unwrap().push(format!("{how} {path} {auth}"));
        let body = format!(r#"{{"items":[{{"name":"{GOLDEN}"}}],"count":1}}"#);
        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        let _ = s.shutdown().await;
    }

    /// An engine on 127.0.0.1 serving `who`'s pair (from [`make_ca`]).
    async fn stand_in(d: &Path, who: &str, mode: Mode) -> (String, Arc<Mutex<Vec<String>>>) {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(d.join(format!("{who}-srv.crt"))).unwrap().map(|c| c.unwrap()).collect();
        let key = PrivateKeyDer::from_pem_file(d.join(format!("{who}-srv.key"))).unwrap();
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = l.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (acceptor, log) = (acceptor.clone(), log.clone());
                tokio::spawn(async move {
                    let mut first = [0u8; 1];
                    let is_tls = matches!(s.peek(&mut first).await, Ok(1)) && first[0] == 0x16;
                    match (mode, is_tls) {
                        (Mode::Tls | Mode::Both, true) => {
                            if let Ok(t) = acceptor.accept(s).await {
                                answer(t, "tls", &log).await;
                            }
                        }
                        (Mode::Plain | Mode::Both, false) => answer(s, "plain", &log).await,
                        // required: no plain; a plain engine: no handshake.
                        _ => drop(s),
                    }
                });
            }
        });
        (at, seen)
    }

    /// Done-when of #81: real handshakes. The test CA's engine is read with
    /// the token over TLS; the same engine is refused under another CA,
    /// before any request or token leaves, as a real answer and not as
    /// infrastructure.
    #[tokio::test]
    async fn an_https_engine_is_verified_against_the_configured_ca() {
        let d = dir("https");
        let ca = make_ca(&d.0, "a");
        let other = make_ca(&d.0, "b");
        let (at, seen) = stand_in(&d.0, "a", Mode::Tls).await;
        let url = format!("https://{at}/");

        let e = resolve_at(Some(&url), &at, Some(&ca), Some("tok"), TIMEOUT, &ONCE).await.unwrap();
        assert_eq!(e.base, format!("https://{at}"), "no trailing slash");
        assert!(crate::census::volume_named(&e.http, &e.base, GOLDEN).await.unwrap());
        assert_eq!(seen.lock().unwrap().as_slice(), ["tls /api/v1/volumes Bearer tok".to_string()]);

        seen.lock().unwrap().clear();
        let e = resolve_at(Some(&url), &at, Some(&other), Some("tok"), TIMEOUT, &ONCE).await.unwrap();
        let err = crate::census::volume_named(&e.http, &e.base, GOLDEN).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid peer certificate"), "{err:#}");
        assert!(!retry::is_infra(&err), "a refused certificate is an answer: {err:#}");
        assert!(seen.lock().unwrap().is_empty(), "nothing was sent to it");
    }

    /// No way to reach an https engine unverified: no CA, a CA that is gone,
    /// or a file with no certificate stop the client before any call. On
    /// plain http the CA is not read at all.
    #[tokio::test]
    async fn an_https_engine_without_a_usable_ca_is_refused() {
        let d = dir("noca");
        let url = Some("https://127.0.0.1:1");
        let at = "127.0.0.1:1";
        let e = resolve_at(url, at, None, None, TIMEOUT, &ONCE).await.expect_err("no CA");
        assert!(e.to_string().contains("needs a CA"), "{e:#}");
        let gone = d.0.join("gone.crt");
        let e = resolve_at(url, at, Some(&gone), None, TIMEOUT, &ONCE).await.expect_err("gone");
        assert!(format!("{e:#}").contains("reading the engine's CA"), "{e:#}");
        let empty = d.0.join("empty.crt");
        std::fs::write(&empty, "").unwrap();
        let e = resolve_at(url, at, Some(&empty), None, TIMEOUT, &ONCE).await.expect_err("empty");
        assert!(format!("{e:#}").contains("no PEM certificate"), "{e:#}");
        assert!(resolve_at(Some("ftp://x"), at, None, None, TIMEOUT, &ONCE).await.is_err());

        let plain = resolve_at(Some("http://127.0.0.1:1"), at, Some(&gone), None, TIMEOUT, &ONCE).await.unwrap();
        assert_eq!(plain.base, "http://127.0.0.1:1");
    }

    /// `http://` keeps working until `required`, with the token.
    #[tokio::test]
    async fn a_plain_url_stays_plain() {
        let d = dir("plain");
        let ca = make_ca(&d.0, "a");
        let (at, seen) = stand_in(&d.0, "a", Mode::Both).await;
        let e = resolve_at(Some(&format!("http://{at}")), &at, Some(&ca), Some("tok"), TIMEOUT, &ONCE).await.unwrap();
        assert!(crate::census::volume_named(&e.http, &e.base, GOLDEN).await.unwrap());
        assert_eq!(seen.lock().unwrap().as_slice(), ["plain /api/v1/volumes Bearer tok".to_string()]);
    }

    /// The runner's case, no URL: `optional` and `required` engines are
    /// reached over TLS, a plain one over http, and asking sends no token.
    #[tokio::test]
    async fn with_no_url_the_engine_is_reached_as_it_answers() {
        let d = dir("auto");
        let ca = make_ca(&d.0, "a");
        for (mode, scheme, how) in [(Mode::Tls, "https", "tls"), (Mode::Both, "https", "tls"), (Mode::Plain, "http", "plain")] {
            let (at, seen) = stand_in(&d.0, "a", mode).await;
            let e = resolve_at(None, &at, Some(&ca), Some("tok"), TIMEOUT, &ONCE).await.unwrap();
            assert_eq!(e.base, format!("{scheme}://{at}"));
            let asked = seen.lock().unwrap().clone();
            assert_eq!(asked, [format!("{how} {HEALTH} ")], "asked once, no token");
            assert!(crate::census::volume_named(&e.http, &e.base, GOLDEN).await.unwrap());
            assert_eq!(seen.lock().unwrap().last().unwrap(), &format!("{how} /api/v1/volumes Bearer tok"));
        }
    }

    /// An engine answering both, whose certificate another CA issued: https
    /// and refused, never the plain port it also offers.
    #[tokio::test]
    async fn a_refused_certificate_never_falls_back_to_plain() {
        let d = dir("refused");
        make_ca(&d.0, "a");
        let other = make_ca(&d.0, "b");
        let (at, seen) = stand_in(&d.0, "a", Mode::Both).await;
        let e = resolve_at(None, &at, Some(&other), Some("tok"), TIMEOUT, &ONCE).await.unwrap();
        assert_eq!(e.base, format!("https://{at}"));
        let err = crate::census::stormblock_health(&e.http, &e.base).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid peer certificate"), "{err:#}");
        assert!(seen.lock().unwrap().is_empty(), "no request, plain or TLS: {:?}", seen.lock().unwrap());
    }

    /// No CA on the node, or an engine not answering now: plain, as before.
    #[tokio::test]
    async fn no_ca_or_no_answer_stays_on_the_plain_default() {
        let e = resolve_at(None, "127.0.0.1:1", None, None, TIMEOUT, &ONCE).await.unwrap();
        assert_eq!(e.base, "http://127.0.0.1:1");
        let d = dir("nothing");
        let ca = make_ca(&d.0, "a");
        let e = resolve_at(None, "127.0.0.1:1", Some(&ca), None, TIMEOUT, &ONCE).await.unwrap();
        assert_eq!(e.base, "http://127.0.0.1:1");
    }
}
