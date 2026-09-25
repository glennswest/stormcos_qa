//! Logging in to a guest, in-process (the image is `scratch`: no ssh
//! binary). A port that answers is not a login, so a check authenticates with
//! the run's key and runs a command. Adapted from stormvm-verify's ssh.rs.
//!
//! The key is an ed25519 pair generated per run. The public half goes to the
//! VMs through `accessCredentials`; the private half never leaves memory.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
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
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, Trusting).await.with_context(|| format!("connecting to {addr}"))?;
    let auth = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
        .await
        .context("offering the key")?;
    anyhow::ensure!(
        auth.success(),
        "{user}@{addr} refused the run's key: sshd is up but the key from accessCredentials is not in the guest"
    );
    let mut channel = session.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut out = Vec::new();
    let mut code = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
            russh::ChannelMsg::ExtendedData { ref data, .. } => out.extend_from_slice(data),
            russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {
                if code.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    anyhow::ensure!(code.unwrap_or(0) == 0, "`{command}` exited {} in the guest: {text}", code.unwrap_or(0));
    Ok(text)
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
}
