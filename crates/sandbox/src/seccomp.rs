//! Linux: a seccomp filter for the sockets Landlock does not see.
//!
//! Landlock's network rules cover TCP `bind` and `connect` and nothing else,
//! so under `network = "deny"` or `"proxy"` a command could still send UDP
//! (DNS included), open a raw or packet socket, or speak a stream protocol
//! that is not TCP (SCTP, MPTCP). This filter closes that by refusing to
//! CREATE such sockets: `socket()` fails with `EACCES`, as Landlock's own
//! refusals do, so programs report "permission denied" rather than crash.
//!
//! It is an allowlist of socket families, not a list of what to block:
//! - `AF_UNIX`: local IPC (an ssh-agent, a build daemon, a language server).
//! - `AF_INET`/`AF_INET6` with `SOCK_STREAM` and protocol 0 or TCP: plain
//!   TCP, which Landlock then confines (nothing, or the proxy's port). The
//!   type's flag bits (`SOCK_NONBLOCK`, `SOCK_CLOEXEC`) are masked off first.
//! - `AF_NETLINK`: the kernel's own message bus. `getaddrinfo` asks it which
//!   addresses the machine has (`AI_ADDRCONFIG`), and `ip`, `ss` and many
//!   runtimes read routes and links through it. What it allows: reading the
//!   machine's interfaces, addresses and routes, and talking to other local
//!   netlink listeners. Changing them needs `CAP_NET_ADMIN`, which a
//!   sandboxed command does not have over the real network (with
//!   `no_new_privs`, even setuid `ip` cannot gain it). Nothing sent on it
//!   leaves the machine, so it is kept; the cost is that a command can list
//!   the host's interfaces, as it could by reading `/proc/net`.
//!
//! Everything else is refused: UDP, ICMP "ping" sockets, raw, `AF_PACKET`,
//! SCTP, MPTCP, and the families no build needs (`AF_VSOCK`, which reaches a
//! VM's host; Bluetooth; CAN; and so on).
//!
//! Two other ways to make a socket are closed with it:
//! - `io_uring` can create sockets and send on them without the syscalls a
//!   seccomp filter sees, so `io_uring_setup` fails with `EPERM` (what
//!   Docker's default profile does; runtimes fall back to plain syscalls).
//! - On x86-64 a 64-bit process can still make 32-bit syscalls. Those are
//!   checked too, and the 32-bit `socketcall(SYS_SOCKET)`, whose arguments
//!   sit in memory a filter cannot read, is refused outright (32-bit glibc
//!   uses the direct `socket` syscall on any current kernel). The x32 ABI
//!   shares x86-64's syscall numbers with one bit set, which is masked.
//!
//! Not covered: `socketpair` (it only makes connected local pairs), sockets
//! passed in already open (`sbx` passes none but stdio), and name lookups a
//! local daemon does on the command's behalf over a Unix socket: with
//! `nss-resolve` (systemd-resolved) or `nscd` in `/etc/nsswitch.conf`,
//! `getaddrinfo` still answers, and the name reaches the resolver.
//!
//! The filter is written by hand: a few dozen BPF instructions do not justify
//! a dependency. It knows x86-64 (with i386 and x32) and aarch64 (with
//! 32-bit ARM); on any other architecture none is installed and the network
//! status says so, because a filter with the wrong syscall numbers would
//! check the wrong calls.

use crate::Status;

/// What the filter covers, for the network status line.
pub(crate) const COVERS: &str = "seccomp: UDP, raw and other non-TCP sockets refused";

// From linux/audit.h: the `arch` field of `seccomp_data`.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
const AUDIT_ARCH_ARM: u32 = 0x4000_0028;

/// x32 syscalls are x86-64's numbers with this bit set.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
/// The kernel's `SOCK_TYPE_MASK`: the type without its flag bits.
const SOCK_TYPE_MASK: u32 = 0xf;
/// `socketcall`'s first argument for `socket`.
const SYS_SOCKET: u32 = 1;
/// `io_uring_setup`, `_enter` and `_register` have one number everywhere.
const IO_URING: [u32; 3] = [425, 426, 427];

/// One syscall ABI the kernel may be entered through.
struct Abi {
    arch: u32,
    socket: u32,
    /// The multiplexed `socketcall`, where the ABI has one.
    socketcall: Option<u32>,
    /// Mask `X32_SYSCALL_BIT` off the syscall number first.
    x32: bool,
}

/// x86-64, and the 32-bit ABIs a 64-bit process can still enter: i386
/// (`int 0x80`) and x32 (x86-64's numbers with a bit set).
const X86_64: &[Abi] = &[
    Abi {
        arch: AUDIT_ARCH_X86_64,
        socket: 41,
        socketcall: None,
        x32: true,
    },
    Abi {
        arch: AUDIT_ARCH_I386,
        socket: 359,
        socketcall: Some(102),
        x32: false,
    },
];

/// aarch64, and 32-bit ARM where the kernel has compat support. EABI has no
/// `socketcall`.
const AARCH64: &[Abi] = &[
    Abi {
        arch: AUDIT_ARCH_AARCH64,
        socket: 198,
        socketcall: None,
        x32: false,
    },
    Abi {
        arch: AUDIT_ARCH_ARM,
        socket: 281,
        socketcall: None,
        x32: false,
    },
];

/// The ABIs a process on this machine can make syscalls through.
const ABIS: Option<&[Abi]> = if cfg!(target_arch = "x86_64") {
    Some(X86_64)
} else if cfg!(target_arch = "aarch64") {
    Some(AARCH64)
} else {
    None
};

/// Whether the filter can be installed here, and if not, why.
pub(crate) fn status() -> Status {
    if ABIS.is_none() {
        return Status::NotEnforced(format!(
            "no seccomp filter for {} yet, so UDP and other non-TCP sockets are not refused",
            std::env::consts::ARCH
        ));
    }
    // Asks whether the kernel knows the ERRNO action, without installing
    // anything; fails on a kernel without seccomp filters (or before 4.14).
    let action: u32 = libc::SECCOMP_RET_ERRNO;
    // SAFETY: SECCOMP_GET_ACTION_AVAIL reads one u32 through the pointer.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_GET_ACTION_AVAIL,
            0,
            &raw const action,
        )
    };
    if rc == 0 {
        Status::Enforced(COVERS.into())
    } else {
        Status::NotEnforced(format!(
            "this kernel has no seccomp filters ({}), so UDP and other non-TCP sockets \
             are not refused",
            std::io::Error::last_os_error()
        ))
    }
}

/// Install the filter on the CALLING thread (the helper, single-threaded,
/// just before it execs); every process it starts inherits it, and it cannot
/// be removed.
///
/// # Errors
/// On an architecture without a filter, or when the kernel refuses it.
pub(crate) fn restrict_sockets() -> Result<(), String> {
    let program =
        program().ok_or_else(|| format!("no seccomp filter for {} yet", std::env::consts::ARCH))?;
    let mut filter: Vec<libc::sock_filter> = program;
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| "seccomp filter too long")?,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: prctl with integer arguments. Landlock already set it, but
    // not if Landlock was skipped (best effort); an unprivileged process
    // may only install a filter with it set.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("no_new_privs: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `prog` points at `filter`, which outlives the call; the kernel
    // copies the program.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &raw const prog,
        )
    };
    if rc != 0 {
        return Err(format!(
            "seccomp filter refused: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

// Offsets into `struct seccomp_data`: nr, arch, then six u64 args. Both
// architectures are little-endian, so an argument's low 32 bits come first,
// and the kernel reads `socket`'s int arguments as exactly those bits.
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}

/// Where a jump goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum To {
    Next,
    Label(&'static str),
}

enum Ins {
    Label(&'static str),
    Load(u32),
    And(u32),
    /// `if A == k goto yes else goto no`.
    Jeq(u32, To, To),
    Ret(u32),
}

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const EACCES: u32 = libc::SECCOMP_RET_ERRNO | libc::EACCES as u32;
const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

/// The BPF program for this architecture, or `None` where there is none.
pub(crate) fn program() -> Option<Vec<libc::sock_filter>> {
    ABIS.map(|abis| assemble(&source(abis)))
}

fn source(abis: &[Abi]) -> Vec<Ins> {
    use Ins::{And, Jeq, Label, Load, Ret};
    use To::{Label as L, Next};
    const ABI_LABELS: [&str; 2] = ["abi0", "abi1"];
    const CALL_LABELS: [&str; 2] = ["socketcall0", "socketcall1"];
    assert!(abis.len() <= ABI_LABELS.len());

    let mut p = vec![Load(ARCH)];
    for (i, abi) in abis.iter().enumerate() {
        p.push(Jeq(abi.arch, L(ABI_LABELS[i]), Next));
    }
    // A syscall ABI this filter does not know: its numbers mean nothing
    // here, so nothing it asks for can be checked. Never seen in practice.
    p.push(Ret(libc::SECCOMP_RET_KILL_PROCESS));

    for (i, abi) in abis.iter().enumerate() {
        p.push(Label(ABI_LABELS[i]));
        p.push(Load(NR));
        if abi.x32 {
            p.push(And(!X32_SYSCALL_BIT));
        }
        p.push(Jeq(abi.socket, L("socket"), Next));
        if let Some(nr) = abi.socketcall {
            p.push(Jeq(nr, L(CALL_LABELS[i]), Next));
        }
        for nr in IO_URING {
            p.push(Jeq(nr, L("eperm"), Next));
        }
        p.push(Ret(ALLOW));
        if abi.socketcall.is_some() {
            p.push(Label(CALL_LABELS[i]));
            p.push(Load(arg(0)));
            p.push(Jeq(SYS_SOCKET, L("eacces"), L("allow")));
        }
    }

    p.extend([
        Label("socket"),
        Load(arg(0)),
        Jeq(libc::AF_UNIX as u32, L("allow"), Next),
        Jeq(libc::AF_NETLINK as u32, L("allow"), Next),
        Jeq(libc::AF_INET as u32, L("inet"), Next),
        Jeq(libc::AF_INET6 as u32, L("inet"), L("eacces")),
        Label("inet"),
        Load(arg(1)),
        And(SOCK_TYPE_MASK),
        Jeq(libc::SOCK_STREAM as u32, Next, L("eacces")),
        Load(arg(2)),
        Jeq(0, L("allow"), Next),
        Jeq(libc::IPPROTO_TCP as u32, L("allow"), L("eacces")),
        Label("allow"),
        Ret(ALLOW),
        Label("eacces"),
        Ret(EACCES),
        Label("eperm"),
        Ret(EPERM),
    ]);
    p
}

/// Resolve labels into BPF's forward-only relative jumps.
fn assemble(source: &[Ins]) -> Vec<libc::sock_filter> {
    let mut at = std::collections::HashMap::new();
    let mut n = 0usize;
    for ins in source {
        match ins {
            Ins::Label(l) => {
                at.insert(*l, n);
            }
            _ => n += 1,
        }
    }
    let offset = |from: usize, to: To| -> u8 {
        match to {
            To::Next => 0,
            To::Label(l) => {
                let target = at[l];
                assert!(target > from, "BPF jumps only forward: {l}");
                u8::try_from(target - from - 1).expect("jump fits in u8")
            }
        }
    };
    let stmt = |code: u32, k: u32| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let mut out = Vec::with_capacity(n);
    for ins in source {
        let i = out.len();
        out.push(match *ins {
            Ins::Label(_) => continue,
            Ins::Load(off) => stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, off),
            Ins::And(k) => stmt(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, k),
            Ins::Ret(k) => stmt(libc::BPF_RET | libc::BPF_K, k),
            Ins::Jeq(k, yes, no) => libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: offset(i, yes),
                jf: offset(i, no),
                k,
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny interpreter for the instructions `assemble` emits, so the
    /// program's decisions can be checked without installing it.
    fn run(prog: &[libc::sock_filter], arch: u32, nr: u32, args: [u32; 3]) -> u32 {
        let load = |off: u32| match off {
            NR => nr,
            ARCH => arch,
            o => args[((o - 16) / 8) as usize],
        };
        let mut a = 0u32;
        let mut pc = 0usize;
        loop {
            let ins = &prog[pc];
            let code = u32::from(ins.code);
            pc += 1;
            if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS {
                a = load(ins.k);
            } else if code == libc::BPF_ALU | libc::BPF_AND | libc::BPF_K {
                a &= ins.k;
            } else if code == libc::BPF_RET | libc::BPF_K {
                return ins.k;
            } else if code == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K {
                pc += usize::from(if a == ins.k { ins.jt } else { ins.jf });
            } else {
                panic!("unexpected instruction {code:#x}");
            }
        }
    }

    const STREAM: u32 = libc::SOCK_STREAM as u32;
    const DGRAM: u32 = libc::SOCK_DGRAM as u32;
    const FLAGS: u32 = (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u32;

    fn check(abi: &Abi) {
        let prog = assemble(&source(std::slice::from_ref(abi)));
        let sock = |family: i32, ty: u32, proto: i32| {
            run(
                &prog,
                abi.arch,
                abi.socket,
                [family as u32, ty, proto as u32],
            )
        };
        let inet = libc::AF_INET;
        let inet6 = libc::AF_INET6;
        assert_eq!(sock(inet, STREAM, 0), ALLOW);
        assert_eq!(sock(inet6, STREAM | FLAGS, libc::IPPROTO_TCP), ALLOW);
        assert_eq!(sock(libc::AF_UNIX, DGRAM | FLAGS, 0), ALLOW);
        assert_eq!(sock(libc::AF_NETLINK, libc::SOCK_RAW as u32, 0), ALLOW);
        assert_eq!(sock(inet, DGRAM, 0), EACCES);
        assert_eq!(sock(inet6, DGRAM | FLAGS, 0), EACCES);
        assert_eq!(sock(inet, DGRAM, libc::IPPROTO_ICMP), EACCES);
        assert_eq!(sock(inet, libc::SOCK_RAW as u32, 0), EACCES);
        assert_eq!(sock(inet, libc::SOCK_SEQPACKET as u32, 0), EACCES);
        assert_eq!(sock(inet, STREAM, libc::IPPROTO_SCTP), EACCES);
        assert_eq!(sock(inet, STREAM, 262), EACCES, "MPTCP");
        assert_eq!(sock(libc::AF_PACKET, libc::SOCK_RAW as u32, 0), EACCES);
        assert_eq!(sock(libc::AF_VSOCK, STREAM, 0), EACCES);
        for nr in IO_URING {
            assert_eq!(run(&prog, abi.arch, nr, [0; 3]), EPERM);
        }
        // Any other syscall passes, whatever its arguments.
        assert_eq!(run(&prog, abi.arch, 0, [2, 2, 0]), ALLOW);
        if let Some(nr) = abi.socketcall {
            assert_eq!(run(&prog, abi.arch, nr, [SYS_SOCKET, 0, 0]), EACCES);
            assert_eq!(run(&prog, abi.arch, nr, [3, 0, 0]), ALLOW, "connect");
        }
        if abi.x32 {
            let nr = abi.socket | X32_SYSCALL_BIT;
            assert_eq!(run(&prog, abi.arch, nr, [inet as u32, DGRAM, 0]), EACCES);
        }
    }

    #[test]
    fn every_known_abi_refuses_what_landlock_misses() {
        for abi in X86_64.iter().chain(AARCH64) {
            check(abi);
        }
    }

    #[test]
    fn the_installed_program_checks_both_abis_and_kills_unknown_ones() {
        let Some(prog) = program() else { return };
        let abis = ABIS.expect("abis");
        for abi in abis {
            let udp = [libc::AF_INET as u32, DGRAM, 0];
            assert_eq!(run(&prog, abi.arch, abi.socket, udp), EACCES);
            let tcp = [libc::AF_INET as u32, STREAM, 0];
            assert_eq!(run(&prog, abi.arch, abi.socket, tcp), ALLOW);
        }
        assert_eq!(
            run(&prog, 0xdead_beef, 0, [0; 3]),
            libc::SECCOMP_RET_KILL_PROCESS
        );
    }
}
