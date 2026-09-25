//! Is a VM's desktop reachable over RDP through stormrdp?
//!
//! stormrdp is one gateway per node on `:3389` that bridges RDP to a VM's
//! VNC socket; it picks the VM from the X.224 Connection Request's routing
//! token (`vm/<ns>/<name>`, what an `.rdp` file's `loadbalanceinfo` carries)
//! and requires NLA. So the probe sends that request offering HYBRID and reads
//! the Connection Confirm:
//!
//! - `RDP_NEG_RSP` — the gateway found the VM and agreed a protocol: **up**;
//! - `RDP_NEG_FAILURE` — it answered but refused (no such target, or no VNC
//!   socket for it): **not up**, with the failure code;
//! - no answer / refused connection — no gateway on the node.
//!
//! This stops before TLS and login: stormrdp's own users are its config, not
//! something a test should hold.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROTOCOL_HYBRID: u32 = 0x2;

/// The TPKT + X.224 Connection Request with a routing token and an
/// RDP_NEG_REQ offering `protocols`.
pub fn connection_request(routing_token: &str, protocols: u32) -> Vec<u8> {
    let mut x224 = vec![0u8, 0xE0, 0, 0, 0, 0, 0]; // LI (filled below), CR, dst-ref, src-ref, class 0
    x224.extend_from_slice(routing_token.as_bytes());
    x224.extend_from_slice(b"\r\n");
    x224.extend_from_slice(&[0x01, 0x00, 0x08, 0x00]); // RDP_NEG_REQ, flags 0, length 8
    x224.extend_from_slice(&protocols.to_le_bytes());
    x224[0] = (x224.len() - 1) as u8;
    let total = (x224.len() + 4) as u16;
    let mut pkt = vec![0x03, 0x00];
    pkt.extend_from_slice(&total.to_be_bytes());
    pkt.extend_from_slice(&x224);
    pkt
}

#[derive(Debug, PartialEq, Eq)]
pub enum Confirm {
    /// Selected protocol.
    Accepted(u32),
    /// RDP_NEG_FAILURE code.
    Refused(u32),
    /// A Connection Confirm without negotiation data (a non-NLA server).
    Plain,
}

pub fn parse_confirm(buf: &[u8]) -> Result<Confirm> {
    anyhow::ensure!(buf.len() >= 11 && buf[0] == 0x03, "not a TPKT: {:02x?}", &buf[..buf.len().min(16)]);
    anyhow::ensure!(buf[5] & 0xF0 == 0xD0, "not an X.224 Connection Confirm (code {:#04x})", buf[5]);
    let neg = &buf[11..];
    if neg.len() < 8 {
        return Ok(Confirm::Plain);
    }
    let value = u32::from_le_bytes([neg[4], neg[5], neg[6], neg[7]]);
    match neg[0] {
        0x02 => Ok(Confirm::Accepted(value)),
        0x03 => Ok(Confirm::Refused(value)),
        t => bail!("unknown negotiation type {t:#04x}"),
    }
}

/// Probe `addr` for VM `ns/name`. `Ok(detail)` when the gateway accepted it.
pub async fn probe(addr: &str, ns: &str, name: &str, timeout: Duration) -> Result<String> {
    let token = format!("vm/{ns}/{name}");
    let fut = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("RDP {addr}: no gateway answering"))?;
        s.write_all(&connection_request(&token, PROTOCOL_HYBRID)).await?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await.context("the gateway closed without a Connection Confirm")?;
        let len = u16::from_be_bytes([head[2], head[3]]) as usize;
        anyhow::ensure!((11..=512).contains(&len), "TPKT length {len} out of range");
        let mut buf = head.to_vec();
        buf.resize(len, 0);
        s.read_exact(&mut buf[4..]).await?;
        match parse_confirm(&buf)? {
            Confirm::Accepted(p) => Ok(format!("{token} accepted by {addr}, protocol {p:#x}")),
            Confirm::Refused(c) => bail!("{addr} refused {token}: RDP_NEG_FAILURE {c:#x} (unknown target or no display)"),
            Confirm::Plain => bail!("{addr} answered without negotiation: not the stormrdp gateway"),
        }
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| anyhow::anyhow!("RDP {addr}: no answer in {}s", timeout.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_well_formed() {
        let p = connection_request("vm/ns/a", PROTOCOL_HYBRID);
        assert_eq!(&p[..2], &[3, 0]);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]) as usize, p.len());
        assert_eq!(p[4] as usize, p.len() - 5);
        assert_eq!(p[5], 0xE0);
        assert!(p.windows(9).any(|w| w == b"vm/ns/a\r\n"));
        assert_eq!(&p[p.len() - 8..], &[1, 0, 8, 0, 2, 0, 0, 0]);
    }

    #[test]
    fn confirms() {
        let mut ok = vec![3, 0, 0, 19, 14, 0xD0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0];
        assert_eq!(parse_confirm(&ok).unwrap(), Confirm::Accepted(2));
        ok[11] = 3;
        ok[15] = 5;
        assert_eq!(parse_confirm(&ok).unwrap(), Confirm::Refused(5));
        assert_eq!(parse_confirm(&[3, 0, 0, 11, 6, 0xD0, 0, 0, 0, 0, 0]).unwrap(), Confirm::Plain);
        assert!(parse_confirm(&[3, 0, 0, 11, 6, 0xE0, 0, 0, 0, 0, 0]).is_err());
    }
}
