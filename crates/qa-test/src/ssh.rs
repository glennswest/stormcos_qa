//! Logging in to a guest, in-process (the image is `scratch`: no ssh
//! binary). A port that answers is not a login, so a check authenticates with
//! the run's key and runs a command. Adapted from stormvm-verify's ssh.rs.
//!
//! The key is an ed25519 pair generated per run. The public half goes to the
//! VMs through `accessCredentials`; the private half never leaves memory.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use retry::Step;
use russh::client;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

pub struct Key {
    pub private: Arc<PrivateKey>,
    /// `ssh-ed25519 AAAA…`
    pub public_openssh: String,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("public_openssh", &self.public_openssh).finish_non_exhaustive()
    }
}

impl Key {
    pub fn generate() -> Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("no entropy from the kernel: {e}"))?;
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
        let public_openssh = key.public_key().to_openssh().context("encoding the public key")?;
        Ok(Key { private: Arc::new(key), public_openssh })
    }

    /// The private half in OpenSSH form, for an in-namespace agent pod
    /// (through a Secret in the run's own namespace, deleted with it).
    pub fn private_openssh(&self) -> Result<String> {
        Ok(self.private.to_openssh(russh::keys::ssh_key::LineEnding::LF).context("encoding the private key")?.to_string())
    }

    pub fn from_openssh(pem: &str) -> Result<Self> {
        let key = PrivateKey::from_openssh(pem).context("parsing the private key")?;
        let public_openssh = key.public_key().to_openssh().context("encoding the public key")?;
        Ok(Key { private: Arc::new(key), public_openssh })
    }
}

struct Trusting;

impl client::Handler for Trusting {
    type Error = russh::Error;

    /// The guest was made by this run moments ago; there is no recorded host
    /// key to compare against. What is proved is that the guest accepts ours.
    async fn check_server_key(&mut self, _key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Log in and run one command; returns its output (stdout+stderr). A
/// non-zero exit is an error carrying that output.
pub async fn run(addr: &str, user: &str, key: Arc<PrivateKey>, command: &str, timeout: Duration) -> Result<String> {
    tokio::time::timeout(timeout, run_inner(addr, user, key, command))
        .await
        .map_err(|_| anyhow::anyhow!("`{command}` on {user}@{addr} did not finish in {}s", timeout.as_secs()))?
}

async fn run_inner(addr: &str, user: &str, key: Arc<PrivateKey>, command: &str) -> Result<String> {
    // Connecting and logging in are retried (#57): a refused or reset
    // connection, or a handshake cut short, is worth another try. A key the
    // guest refuses is its answer. The command itself is never re-run (it
    // may have acted).
    let session = retry::with_backoff(&retry::Policy::CONNECT, &format!("ssh {user}@{addr}"), |_| {
        let key = key.clone();
        async move {
            let config = Arc::new(client::Config::default());
            let mut session = client::connect(config, addr, Trusting).await.map_err(|e| Step::transient(format!("connecting to {addr}: {e}")))?;
            let auth = session
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
                .await
                .map_err(|e| Step::transient(format!("offering the key: {e}")))?;
            if !auth.success() {
                return Err(Step::Final(anyhow::anyhow!(
                    "{user}@{addr} refused the run's key: sshd is up but the key from accessCredentials is not in the guest"
                )));
            }
            Ok(session)
        }
    })
    .await?;
    let mut channel = session.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut out = Vec::new();
    let mut code = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
            russh::ChannelMsg::ExtendedData { ref data, .. } => out.extend_from_slice(data),
            russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close if code.is_some() => break,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    anyhow::ensure!(code.unwrap_or(0) == 0, "`{command}` exited {} in the guest: {text}", code.unwrap_or(0));
    Ok(text)
}

/// Until `deadline`: log in with `key` and run `true`, every 3 s. A VMI that
/// is Running is not a booted guest; this is the wait for its sshd (vm-waves
/// `up`, medium `up/vm-*`, #91). The error is the last try's.
pub async fn wait_login(addr: &str, user: &str, key: Arc<PrivateKey>, deadline: std::time::Instant) -> Result<()> {
    let mut last = anyhow::anyhow!("never tried");
    while std::time::Instant::now() < deadline {
        match run(addr, user, key.clone(), "true", Duration::from_secs(20)).await {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(last.context("ssh never accepted the run's key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_one_line_and_per_run() {
        let a = Key::generate().unwrap();
        let b = Key::generate().unwrap();
        assert!(a.public_openssh.starts_with("ssh-ed25519 ") && !a.public_openssh.contains('\n'));
        assert_ne!(a.public_openssh, b.public_openssh);
    }

    #[test]
    fn a_key_survives_the_trip_to_an_agent() {
        let a = Key::generate().unwrap();
        let b = Key::from_openssh(&a.private_openssh().unwrap()).unwrap();
        assert_eq!(a.public_openssh, b.public_openssh);
    }

    /// A guest whose sshd is not up yet is tried again until the deadline,
    /// and the failure says it never logged in, with the last try's cause.
    #[tokio::test]
    async fn a_login_that_never_comes_is_waited_for_then_said() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(); // closed once dropped
        let key = Key::generate().unwrap();
        let t0 = std::time::Instant::now();
        let e = wait_login(&format!("127.0.0.1:{port}"), "fedora", key.private.clone(), t0 + Duration::from_secs(4)).await.unwrap_err();
        assert!(t0.elapsed() >= Duration::from_secs(3), "tried again, not once");
        let s = format!("{e:#}");
        assert!(s.starts_with("ssh never accepted the run's key: ") && !s.contains("never tried"), "{s}");
        let e = wait_login("127.0.0.1:1", "fedora", key.private, std::time::Instant::now()).await.unwrap_err();
        assert_eq!(format!("{e:#}"), "ssh never accepted the run's key: never tried");
    }
}
