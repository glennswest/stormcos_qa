//! A crash names its place (#26). The first live `turbomode` runs died with
//! a bare SIGSEGV (exit 139) and nothing else; this prints the signal, the
//! fault address, the thread and a symbolized backtrace to stderr first.
//!
//! The lines carry no spaces: the kubelet's `/log` strips the first three
//! words of a line with three or more (rustkube-node#136).
//!
//! Not async-signal-safe past the first line (the backtrace allocates), which
//! is acceptable for a process that is about to die: an alarm ends a handler
//! that hangs (exit 142), and the signal's default action ends one that
//! returns (the fault repeats, exit 139 as before).

use std::fmt::Write as _;

fn raw(msg: &[u8]) {
    // SAFETY: write(2) on stderr from a buffer we own.
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
    }
}

/// `n` in hex into `buf`, without allocating; the digits written.
fn hex(mut n: usize, buf: &mut [u8; 18]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b"0123456789abcdef"[n & 15];
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    i -= 1;
    buf[i] = b'x';
    i -= 1;
    buf[i] = b'0';
    &buf[i..]
}

/// The backtrace as lines without spaces (see the module comment).
pub fn report_lines(bt: &str) -> String {
    let mut out = String::new();
    for line in bt.lines() {
        let line = line.trim();
        if !line.is_empty() {
            let _ = writeln!(out, "crash:bt:{}", line.replace(' ', "_"));
        }
    }
    out
}

extern "C" fn on_fault(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for an SA_SIGINFO handler.
    let addr = unsafe { (*info).si_addr() } as usize;
    let mut buf = [0u8; 18];
    raw(if sig == libc::SIGBUS { b"crash:SIGBUS:addr=" } else { b"crash:SIGSEGV:addr=" });
    raw(hex(addr, &mut buf));
    raw(b"\n");
    // SAFETY: alarm(2) and signal(2) with the default action.
    unsafe {
        libc::signal(libc::SIGALRM, libc::SIG_DFL);
        libc::alarm(10);
    }
    let thread = std::thread::current().name().unwrap_or("unnamed").replace(' ', "_");
    raw(format!("crash:thread={thread}\n").as_bytes());
    raw(report_lines(&std::backtrace::Backtrace::force_capture().to_string()).as_bytes());
    // SAFETY: back to the default action; returning re-runs the faulting
    // instruction, which then ends the process with the signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
}

/// Install the handler for SIGSEGV and SIGBUS. It runs on the alternate
/// signal stack std gives every thread, so it works on a stack overflow too.
pub fn install() {
    for sig in [libc::SIGSEGV, libc::SIGBUS] {
        // SAFETY: a zeroed sigaction with our handler, SA_SIGINFO|SA_ONSTACK.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_fault as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// `/test crash`: a write through a null pointer, for checking the report.
#[inline(never)]
pub fn fault() -> i32 {
    let p = std::hint::black_box(std::ptr::null_mut::<u8>());
    // SAFETY: none — this faults on purpose.
    unsafe { p.write_volatile(1) };
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_lines_survive_the_log_reader() {
        let r = report_lines("   0: turbomode::list\n             at src/x.rs:1\n\n   1: main\n");
        assert_eq!(r, "crash:bt:0:_turbomode::list\ncrash:bt:at_src/x.rs:1\ncrash:bt:1:_main\n");
        assert!(r.lines().all(|l| !l.contains(' ')));
        let mut b = [0u8; 18];
        assert_eq!(hex(0x7f00_1234, &mut b), b"0x7f001234");
        assert_eq!(hex(0, &mut b), b"0x0");
    }
}
