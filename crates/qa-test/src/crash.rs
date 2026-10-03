//! A crash names its place (#26). The first live `turbomode` runs died with
//! a bare SIGSEGV (exit 139) and nothing else. This handler prints the
//! signal, the fault address, the thread, the faulting instruction and the
//! frame-pointer chain (`test/build.sh` builds with frame pointers) to
//! stderr, then lets the signal end the process as before (exit 139).
//!
//! The addresses are raw: `tools/symbolize-crash.sh` rebuilds the commit
//! (`test/build.sh` is reproducible: paths are remapped) and names them, using
//! `crash:anchor`, the address of [`install`], to find the load base.
//! std's own backtrace is not used: capturing it from the handler faulted.
//!
//! Every line is written as it is made, without allocating, so a walk that
//! faults part way (the signal is blocked in the handler, so that ends the
//! process) still leaves what it had. No line has a space: the kubelet's
//! `/log` strips the first three words of a line with three or more
//! (rustkube-node#136).

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

/// `crash:<key>=0x<n>` on one line.
fn line(key: &[u8], n: usize) {
    let mut buf = [0u8; 18];
    raw(b"crash:");
    raw(key);
    raw(b"=");
    raw(hex(n, &mut buf));
    raw(b"\n");
}

/// The frame-pointer chain from `fp`: each frame's return address, while
/// the chain climbs the stack.
fn walk(mut fp: usize, mut emit: impl FnMut(usize), read: impl Fn(usize) -> usize) {
    for _ in 0..64 {
        if fp == 0 || fp % 8 != 0 {
            return;
        }
        let ret = read(fp + 8);
        if ret == 0 {
            return;
        }
        emit(ret);
        let next = read(fp);
        if next <= fp {
            return;
        }
        fp = next;
    }
}

extern "C" fn on_fault(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    raw(if sig == libc::SIGBUS { b"crash:signal=SIGBUS\n" } else { b"crash:signal=SIGSEGV\n" });
    // SAFETY: the kernel passes a valid siginfo for an SA_SIGINFO handler.
    line(b"addr", unsafe { (*info).si_addr() } as usize);
    line(b"anchor", install as *const () as usize);
    // SAFETY: for SA_SIGINFO the third argument is the interrupted ucontext.
    let g = unsafe { (*ctx.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
    line(b"rip", g[libc::REG_RIP as usize] as usize);
    line(b"rsp", g[libc::REG_RSP as usize] as usize);
    // SAFETY: reads along the interrupted thread's frame-pointer chain; a bad
    // pointer faults with the signal blocked, which ends the process.
    walk(g[libc::REG_RBP as usize] as usize, |r| line(b"ret", r), |a| unsafe { *(a as *const usize) });
    if let Some(name) = std::thread::current().name() {
        raw(b"crash:thread=");
        raw(name.replace(' ', "_").as_bytes());
        raw(b"\n");
    }
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
    fn hex_without_allocating() {
        let mut b = [0u8; 18];
        assert_eq!(hex(0x7f00_1234, &mut b), b"0x7f001234");
        assert_eq!(hex(0, &mut b), b"0x0");
        assert_eq!(hex(usize::MAX, &mut b), b"0xffffffffffffffff");
    }

    #[test]
    fn the_walk_follows_the_chain_up_the_stack_and_stops() {
        // fp 0x100 → ret 0xa, next 0x200 → ret 0xb, next 0x100 (down: stop)
        let mem = |a: usize| match a {
            0x100 => 0x200,
            0x108 => 0xa,
            0x200 => 0x100,
            0x208 => 0xb,
            _ => 0,
        };
        let mut got = vec![];
        walk(0x100, |r| got.push(r), mem);
        assert_eq!(got, [0xa, 0xb]);
        got.clear();
        walk(0x101, |r| got.push(r), mem);
        assert!(got.is_empty(), "a misaligned frame pointer is not followed");
    }
}
