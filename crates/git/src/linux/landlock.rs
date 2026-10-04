//! Landlock by raw syscalls (`<linux/landlock.h>`), as sandcastle does
//! its seccomp: a ruleset is built in the parent from a `jail::Plan`, once
//! per repository, and each child only calls `landlock_restrict_self` on
//! it between its fork and its exec, which allocates nothing.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use crate::jail::{access, Plan, LANDLOCK_ABI_MIN};

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's Landlock ABI version, or 0 where it has none.
pub fn abi() -> u32 {
    // SAFETY: the version query takes a null attribute and no size.
    let r = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
    if r < 0 {
        0
    } else {
        r as u32
    }
}

/// A built ruleset, shared by every process of one repository.
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    pub abi: u32,
}

impl Ruleset {
    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Builds the ruleset for `plan` under the kernel's ABI: every filesystem
/// right the ABI knows is handled (so anything no rule grants is
/// refused), TCP bind and connect are handled with no rule (refused) from
/// ABI 4, and abstract unix sockets and signals are scoped from ABI 6.
pub fn build(plan: &Plan) -> io::Result<Ruleset> {
    let abi = abi();
    if abi < LANDLOCK_ABI_MIN {
        return Err(io::Error::other("this kernel has no Landlock"));
    }
    let handled_fs = access::handled(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled_fs,
        handled_access_net: if abi >= 4 { access::NET_BIND_TCP | access::NET_CONNECT_TCP } else { 0 },
        scoped: if abi >= 6 { access::SCOPE_ABSTRACT_UNIX_SOCKET | access::SCOPE_SIGNAL } else { 0 },
    };
    // An older kernel reads only the fields it knows; the size says which.
    let size = if abi >= 6 {
        24
    } else if abi >= 4 {
        16
    } else {
        8
    };
    // SAFETY: a valid attribute of at least `size` bytes.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size as usize, 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a new descriptor we now own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    for rule in &plan.rules {
        let wanted = rule.access & handled_fs & if rule.file { access::FILE_ONLY } else { u64::MAX };
        let c = CString::new(rule.path.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path with NUL"))?;
        // SAFETY: a NUL-terminated path; O_PATH opens without reading.
        let parent = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("opening {} for a Landlock rule: {e}", rule.path.display())));
        }
        // SAFETY: we own the descriptor open just returned.
        let parent = unsafe { OwnedFd::from_raw_fd(parent) };
        let beneath = PathBeneathAttr { allowed_access: wanted, parent_fd: parent.as_raw_fd() };
        // SAFETY: a valid ruleset, rule type, and attribute.
        let r = unsafe { libc::syscall(libc::SYS_landlock_add_rule, fd.as_raw_fd(), RULE_PATH_BENEATH, &beneath as *const PathBeneathAttr, 0u32) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("a Landlock rule for {}: {e}", rule.path.display())));
        }
    }
    Ok(Ruleset { fd, abi })
}

/// In the child, after `PR_SET_NO_NEW_PRIVS`: enforce the ruleset on this
/// process and all it runs. Async-signal-safe: one syscall.
pub fn restrict_self(ruleset: RawFd) -> io::Result<()> {
    // SAFETY: a ruleset descriptor inherited from the parent.
    let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the attribute layouts are the kernel's. Method: their sizes.
    #[test]
    fn layouts() {
        assert_eq!(std::mem::size_of::<RulesetAttr>(), 24);
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
    }

    // Goal: this kernel has Landlock, at the ABI the design assumes for
    // its network rules (the machine the evidence came from: Linux 7.2).
    // Method: the version query.
    #[test]
    fn this_kernel_has_landlock() {
        assert!(abi() >= 4, "Landlock ABI {} (need 4 for TCP rules)", abi());
    }
}
