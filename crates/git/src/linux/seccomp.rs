//! The git process's seccomp filter, after sandcastle's jailer filter
//! (`crates/vm/src/linux/seccomp.rs` there): a denylist of what git never
//! needs and an escape would, plus the network: `socket` for any family
//! but `AF_UNIX`, and io_uring (which could connect without `socket`'s
//! check), answer `EPERM`. `clone3` answers `ENOSYS` so the C library
//! falls back to `clone`, whose flags a filter can read, and `clone` with
//! a namespace flag answers `EPERM`. Everything else is allowed: git runs
//! its own helpers (`pack-objects`, `index-pack`) with `execve`.
//!
//! The program is built as data before the fork (tested on any host) and
//! installed after it without allocating.

use std::io;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
/// x86_64's x32 ABI shares the arch value; its syscalls carry this bit.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0_LOW: u32 = 16;
/// A BPF program's instructions, as the kernel limits them.
pub const INSTRUCTIONS_MAX: usize = 4096;

const NAMESPACE_FLAGS: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWCGROUP) as u32;

/// Refused with `EPERM`.
pub const DENIED: &[libc::c_long] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_reboot,
    libc::SYS_acct,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    libc::SYS_syslog,
    libc::SYS_quotactl,
    libc::SYS_userfaultfd,
    libc::SYS_fanotify_init,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
];

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The program, built as data so its shape is tested on any host.
pub fn program() -> Vec<libc::sock_filter> {
    let eperm = RET_ERRNO | libc::EPERM as u32;
    let enosys = RET_ERRNO | libc::ENOSYS as u32;
    let mut p = vec![
        stmt(LD_W_ABS, OFFSET_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFFSET_NR),
        jump(JSET_K, X32_SYSCALL_BIT, 0, 1),
        stmt(RET_K, enosys),
    ];
    for nr in DENIED {
        p.push(jump(JEQ_K, *nr as u32, 0, 1));
        p.push(stmt(RET_K, eperm));
    }
    p.push(jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1));
    p.push(stmt(RET_K, enosys));
    // clone: allowed unless it asks for a namespace.
    p.push(jump(JEQ_K, libc::SYS_clone as u32, 0, 4));
    p.push(stmt(LD_W_ABS, OFFSET_ARG0_LOW));
    p.push(jump(JSET_K, NAMESPACE_FLAGS, 0, 1));
    p.push(stmt(RET_K, eperm));
    p.push(stmt(RET_K, RET_ALLOW));
    // socket: AF_UNIX only.
    p.push(jump(JEQ_K, libc::SYS_socket as u32, 0, 4));
    p.push(stmt(LD_W_ABS, OFFSET_ARG0_LOW));
    p.push(jump(JEQ_K, libc::AF_UNIX as u32, 1, 0));
    p.push(stmt(RET_K, eperm));
    p.push(stmt(RET_K, RET_ALLOW));
    p.push(stmt(RET_K, RET_ALLOW));
    assert!(p.len() <= INSTRUCTIONS_MAX);
    p
}

/// Installs a program made before the fork; the caller has set
/// no-new-privileges. Async-signal-safe: one `prctl`.
pub fn install(prog: &[libc::sock_filter]) -> io::Result<()> {
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut libc::sock_filter };
    // SAFETY: a valid program that outlives the call (the kernel copies it).
    let r = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const libc::sock_fprog) };
    if r == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: every jump lands inside the program, and every path ends in a
    // return. Method: walk the program's jumps.
    #[test]
    fn jumps_land_inside() {
        let p = program();
        for (i, ins) in p.iter().enumerate() {
            if ins.code == JEQ_K || ins.code == JSET_K {
                assert!(i + 1 + (ins.jt as usize) < p.len());
                assert!(i + 1 + (ins.jf as usize) < p.len());
            }
        }
        assert_eq!(p.last().map(|i| (i.code, i.k)), Some((RET_K, RET_ALLOW)));
    }

    // Goal: the network is refused and git's own needs are not. Method: a
    // tiny interpreter runs the program on (syscall, arg0).
    #[test]
    fn the_filter_refuses_the_network_and_allows_git() {
        let p = program();
        let run = |nr: libc::c_long, arg0: u32| -> u32 {
            let (mut pc, mut acc) = (0usize, 0u32);
            // Bounded by the program's length: every jump goes forward.
            loop {
                let ins = p[pc];
                match ins.code {
                    LD_W_ABS => {
                        acc = match ins.k {
                            OFFSET_ARCH => AUDIT_ARCH,
                            OFFSET_NR => nr as u32,
                            OFFSET_ARG0_LOW => arg0,
                            _ => unreachable!(),
                        };
                        pc += 1;
                    }
                    JEQ_K => pc += 1 + if acc == ins.k { ins.jt } else { ins.jf } as usize,
                    JSET_K => pc += 1 + if acc & ins.k != 0 { ins.jt } else { ins.jf } as usize,
                    RET_K => return ins.k,
                    _ => unreachable!(),
                }
            }
        };
        let eperm = RET_ERRNO | libc::EPERM as u32;
        assert_eq!(run(libc::SYS_socket, libc::AF_INET as u32), eperm);
        assert_eq!(run(libc::SYS_socket, libc::AF_INET6 as u32), eperm);
        assert_eq!(run(libc::SYS_socket, libc::AF_NETLINK as u32), eperm);
        assert_eq!(run(libc::SYS_socket, libc::AF_UNIX as u32), RET_ALLOW);
        assert_eq!(run(libc::SYS_io_uring_setup, 0), eperm);
        assert_eq!(run(libc::SYS_ptrace, 0), eperm);
        assert_eq!(run(libc::SYS_clone, libc::CLONE_NEWUSER as u32), eperm);
        assert_eq!(run(libc::SYS_clone, libc::SIGCHLD as u32), RET_ALLOW);
        assert_eq!(run(libc::SYS_clone3, 0), RET_ERRNO | libc::ENOSYS as u32);
        for nr in [libc::SYS_execve, libc::SYS_openat, libc::SYS_read, libc::SYS_write, libc::SYS_mmap, libc::SYS_fsync, libc::SYS_renameat2] {
            assert_eq!(run(nr, 0), RET_ALLOW, "syscall {nr}");
        }
    }
}
