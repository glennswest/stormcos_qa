//! Bytes through a pod log. The host collector has no other way out: the
//! kubelet has no exec (stormpump#103) and the apiserver no node proxy
//! (rustkube#108), so it prints its tarball and the workstation reads the
//! pod's log.
//!
//! rustkube-node cuts the first three words of any log line with three or
//! more spaces (rustkube-node#136), and stormpump writes its own warnings to
//! the same log. So the frame has no spaces at all:
//!
//! ```text
//! mg:begin:<bytes>:<sha256 hex>
//! <base64, 76 per line>
//! mg:end
//! ```
//!
//! and the reader keeps only base64 lines between the markers, then checks
//! length and digest: a line lost or changed fails loudly, never silently.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

const BEGIN: &str = "mg:begin:";
const END: &str = "mg:end";

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// The framed text for `data`, ending in a newline.
pub fn encode(data: &[u8]) -> String {
    let b64 = STANDARD.encode(data);
    let mut out = format!("{BEGIN}{}:{}\n", data.len(), sha256_hex(data));
    for chunk in b64.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(END);
    out.push('\n');
    out
}

/// The bytes framed in `log`, checked; the last frame wins (a restarted
/// container prints again).
pub fn decode(log: &str) -> Result<Vec<u8>, String> {
    let mut found: Option<Result<Vec<u8>, String>> = None;
    let mut lines = log.lines();
    while let Some(l) = lines.next() {
        let Some(head) = l.trim().strip_prefix(BEGIN) else { continue };
        let Some((len, sum)) = head.split_once(':') else {
            found = Some(Err(format!("bad frame header {l:?}")));
            continue;
        };
        let Ok(len) = len.parse::<usize>() else {
            found = Some(Err(format!("bad frame length in {l:?}")));
            continue;
        };
        let mut b64 = String::new();
        let mut ended = false;
        for l in lines.by_ref() {
            let l = l.trim();
            if l == END {
                ended = true;
                break;
            }
            if !l.is_empty() && l.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=')) {
                b64.push_str(l);
            }
        }
        found = Some(if !ended {
            Err("the frame never ended (the log was cut short)".into())
        } else {
            match STANDARD.decode(&b64) {
                Err(e) => Err(format!("base64: {e}")),
                Ok(d) if d.len() != len => Err(format!("{} bytes arrived, {len} were sent", d.len())),
                Ok(d) if sha256_hex(&d) != sum => Err("the digest does not match: a line was lost or changed".into()),
                Ok(d) => Ok(d),
            }
        });
    }
    found.unwrap_or_else(|| Err("no mg:begin frame in the log".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_survives_noise_and_the_136_cut() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let framed = encode(&data);
        assert!(framed.lines().all(|l| !l.contains(' ')), "a frame line has a space");
        // stormpump's warnings around and inside it, as the pod log has them.
        let mut log = String::from("warning: /proc could not be mounted (is the directory in the image?)\n");
        for (i, l) in framed.lines().enumerate() {
            log.push_str(l);
            log.push('\n');
            if i == 3 {
                log.push_str("stormpump: a warning with several spaces in it\n");
            }
        }
        assert_eq!(decode(&log).unwrap(), data);
    }

    #[test]
    fn a_lost_line_or_a_cut_log_fails() {
        let data = vec![42u8; 1000];
        let framed = encode(&data);
        let mut lines: Vec<&str> = framed.lines().collect();
        lines.remove(2);
        assert!(decode(&lines.join("\n")).unwrap_err().contains("arrived"));
        let cut: String = framed.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(decode(&cut).unwrap_err().contains("never ended"));
        assert!(decode("nothing here").is_err());
        let mut changed = framed.clone().into_bytes();
        let at = framed.find('\n').unwrap() + 5;
        changed[at] = if changed[at] == b'A' { b'B' } else { b'A' };
        assert!(decode(&String::from_utf8(changed).unwrap()).unwrap_err().contains("digest"));
    }

    #[test]
    fn the_last_frame_wins() {
        let log = format!("{}{}", encode(b"first"), encode(b"second"));
        assert_eq!(decode(&log).unwrap(), b"second");
    }
}
