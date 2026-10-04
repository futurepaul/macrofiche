//! A git process's jail, as pure data (docs/design.md, The jail): what it
//! may read, run and write, the network it may not reach, and the limits
//! it runs under. Checked here, so a plan that would leak is refused
//! before any process exists; applied by `linux::spawn` after the fork.
//!
//! The model is unprivileged, unlike sandcastle's jailer (a VMM needs
//! namespaces and root; git needs neither): Landlock confines the
//! filesystem to the git prefix, the C library, and the repository; a
//! seccomp filter and Landlock's network rules keep it off the network;
//! no new privileges; rlimits; its own process group; a deadline.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Landlock's filesystem rights (`<linux/landlock.h>`), by the ABI that
/// added them.
pub mod access {
    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const READ_DIR: u64 = 1 << 3;
    pub const REMOVE_DIR: u64 = 1 << 4;
    pub const REMOVE_FILE: u64 = 1 << 5;
    pub const MAKE_CHAR: u64 = 1 << 6;
    pub const MAKE_DIR: u64 = 1 << 7;
    pub const MAKE_REG: u64 = 1 << 8;
    pub const MAKE_SOCK: u64 = 1 << 9;
    pub const MAKE_FIFO: u64 = 1 << 10;
    pub const MAKE_BLOCK: u64 = 1 << 11;
    pub const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2.
    pub const REFER: u64 = 1 << 13;
    /// ABI 3.
    pub const TRUNCATE: u64 = 1 << 14;
    /// ABI 5.
    pub const IOCTL_DEV: u64 = 1 << 15;

    /// Every right an ABI knows.
    pub fn handled(abi: u32) -> u64 {
        let mut all = (1 << 13) - 1;
        if abi >= 2 {
            all |= REFER;
        }
        if abi >= 3 {
            all |= TRUNCATE;
        }
        if abi >= 5 {
            all |= IOCTL_DEV;
        }
        all
    }

    /// The rights that apply to a file (not a directory).
    pub const FILE_ONLY: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

    /// Read and run: the git prefix and the C library.
    pub const READ_EXEC: u64 = EXECUTE | READ_FILE | READ_DIR;
    /// Everything git does in its repository, but run, or make a device,
    /// a socket, a FIFO, or a symlink.
    pub const REPO: u64 = WRITE_FILE | READ_FILE | READ_DIR | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG | REFER | TRUNCATE;

    /// Landlock's network rights (ABI 4): TCP bind and connect.
    pub const NET_BIND_TCP: u64 = 1 << 0;
    pub const NET_CONNECT_TCP: u64 = 1 << 1;
    /// Landlock's scopes (ABI 6): abstract unix sockets and signals
    /// outside the domain.
    pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    pub const SCOPE_SIGNAL: u64 = 1 << 1;
}

/// The lowest Landlock ABI the jail accepts. ABI 4 (Linux 6.7) is the
/// first that refuses TCP; below it the seccomp filter alone keeps git
/// off the network, which `Settings::landlock_abi_min` may allow.
pub const LANDLOCK_ABI_MIN: u32 = 1;
/// Path rules per jail: the prefix, the C library's places, a few device
/// files, and the repository.
pub const RULES_MAX: usize = 16;

/// What a node allows its git processes, fixed at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The git prefix (`target/engine/git-2.55.0`): read and run.
    pub prefix: PathBuf,
    /// Read and run: the C library and the dynamic loader's places.
    pub system_dirs: Vec<PathBuf>,
    /// Read only: single files git needs (`/etc/ld.so.cache`).
    pub system_files: Vec<PathBuf>,
    /// Whether the jail is applied. Off only where Landlock does not exist
    /// (a host that is not Linux, for development): the service refuses to
    /// start with it off unless its config says so.
    pub enforce: bool,
}

impl Settings {
    /// The defaults for a Linux node: the C library under `/usr`, the
    /// loader's cache.
    pub fn linux(prefix: PathBuf) -> Settings {
        Settings { prefix, system_dirs: vec!["/usr".into()], system_files: vec!["/etc/ld.so.cache".into()], enforce: true }
    }
}

/// One process's resource limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_CPU`, seconds of CPU.
    pub cpu_s: u64,
    /// `RLIMIT_DATA`: heap and private writable maps. File-backed maps
    /// (git's pack windows) are not counted, so a large read stays cheap.
    pub data_bytes: u64,
    /// `RLIMIT_FSIZE`: the largest file it may write.
    pub fsize_bytes: u64,
    /// `RLIMIT_NOFILE`.
    pub nofile: u64,
}

impl Limits {
    /// A read or a small write.
    pub const OPERATION: Limits = Limits { cpu_s: 60, data_bytes: 1 << 30, fsize_bytes: 4 << 30, nofile: 1024 };
    /// Maintenance: a repack of a large repository.
    pub const MAINTENANCE: Limits = Limits { cpu_s: 1200, data_bytes: 4 << 30, fsize_bytes: 16 << 30, nofile: 4096 };

    pub fn validate(&self) -> Result<(), JailError> {
        if self.cpu_s == 0 || self.data_bytes < (64 << 20) || self.fsize_bytes < (1 << 20) || self.nofile < 64 {
            return Err(JailError::Limits("cpu_s > 0, data_bytes ≥ 64 MiB, fsize_bytes ≥ 1 MiB, nofile ≥ 64"));
        }
        Ok(())
    }
}

/// One Landlock path rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub path: PathBuf,
    pub access: u64,
    /// A file, so only `access::FILE_ONLY` rights apply.
    pub file: bool,
}

/// The jail of one repository's processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub rules: Vec<Rule>,
    /// The repository: git's `GIT_DIR`, and its `TMPDIR` below it.
    pub repo: PathBuf,
    pub tmp: PathBuf,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JailError {
    #[error("{0}: not an absolute, normal path")]
    Path(PathBuf),
    #[error("{0} is inside the git prefix or a system directory")]
    Overlap(PathBuf),
    #[error("limits: {0}")]
    Limits(&'static str),
    #[error("{0} path rules pass the limit")]
    TooManyRules(usize),
}

/// Absolute, with no `.`, `..`, or empty components, and not `/` itself.
pub fn normal_absolute(p: &Path) -> bool {
    p.is_absolute() && p != Path::new("/") && p.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// The jail for processes working in `repo`: read and run the prefix and
/// the system's places, read the system's files and `/dev/urandom`, read
/// and write `/dev/null`, and every file right but running in the
/// repository. `extra_rw` names more directories a process may write (a
/// receive-pack's spool, later); each must sit outside the prefix and
/// the system's places.
pub fn plan(settings: &Settings, repo: &Path, extra_rw: &[PathBuf]) -> Result<Plan, JailError> {
    for p in std::iter::once(&settings.prefix).chain(&settings.system_dirs).chain(&settings.system_files) {
        if !normal_absolute(p) {
            return Err(JailError::Path(p.clone()));
        }
    }
    let read_only = || std::iter::once(&settings.prefix).chain(&settings.system_dirs);
    let mut rules = Vec::new();
    for d in read_only() {
        rules.push(Rule { path: d.clone(), access: access::READ_EXEC, file: false });
    }
    for f in &settings.system_files {
        rules.push(Rule { path: f.clone(), access: access::READ_FILE, file: true });
    }
    rules.push(Rule { path: "/dev/null".into(), access: access::READ_FILE | access::WRITE_FILE | access::TRUNCATE, file: true });
    rules.push(Rule { path: "/dev/urandom".into(), access: access::READ_FILE, file: true });
    for w in std::iter::once(&repo.to_path_buf()).chain(extra_rw) {
        if !normal_absolute(w) {
            return Err(JailError::Path(w.clone()));
        }
        // A writable place inside a read-only one would let git change
        // what it runs; one around it would let git write the prefix.
        if read_only().any(|r| w.starts_with(r) || r.starts_with(w)) {
            return Err(JailError::Overlap(w.clone()));
        }
        rules.push(Rule { path: w.clone(), access: access::REPO, file: false });
    }
    if rules.len() > RULES_MAX {
        return Err(JailError::TooManyRules(rules.len()));
    }
    let plan = Plan { rules, repo: repo.to_path_buf(), tmp: repo.join("macrofiche/tmp") };
    for r in &plan.rules {
        assert!(!r.file || r.access & !access::FILE_ONLY == 0, "a file rule holds only file rights");
        assert!(r.access & access::EXECUTE == 0 || r.access & access::WRITE_FILE == 0, "nothing is both runnable and writable");
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::linux("/opt/macrofiche/git-2.55.0".into())
    }

    // Goal: a repository's plan reads and runs only the prefix and the
    // system's places, and writes only the repository. Method: the plan's
    // rules, by right.
    #[test]
    fn a_plan_writes_only_its_repository() {
        let p = plan(&settings(), Path::new("/var/lib/macrofiche/repos/o/r.git"), &[]).unwrap();
        let writable: Vec<_> = p.rules.iter().filter(|r| r.access & access::WRITE_FILE != 0 && !r.file).map(|r| r.path.clone()).collect();
        assert_eq!(writable, vec![PathBuf::from("/var/lib/macrofiche/repos/o/r.git")]);
        let runnable: Vec<_> = p.rules.iter().filter(|r| r.access & access::EXECUTE != 0).map(|r| r.path.clone()).collect();
        assert_eq!(runnable, vec![PathBuf::from("/opt/macrofiche/git-2.55.0"), PathBuf::from("/usr")]);
        assert_eq!(p.tmp, Path::new("/var/lib/macrofiche/repos/o/r.git/macrofiche/tmp"));
        assert!(p.rules.iter().any(|r| r.path == Path::new("/dev/null") && r.file));
        assert!(!p.rules.iter().any(|r| r.access & (access::MAKE_SYM | access::MAKE_SOCK | access::MAKE_FIFO | access::MAKE_CHAR | access::MAKE_BLOCK) != 0));
    }

    // Goal: each way a plan could leak is refused. Method: a repository
    // that is relative, has `..`, is `/`, sits in the prefix or `/usr`, or
    // holds the prefix.
    #[test]
    fn leaking_plans_are_refused() {
        let s = settings();
        assert_eq!(plan(&s, Path::new("repos/r.git"), &[]), Err(JailError::Path("repos/r.git".into())));
        assert_eq!(plan(&s, Path::new("/var/../etc"), &[]), Err(JailError::Path("/var/../etc".into())));
        assert_eq!(plan(&s, Path::new("/"), &[]), Err(JailError::Path("/".into())));
        assert_eq!(plan(&s, Path::new("/usr/lib/x.git"), &[]), Err(JailError::Overlap("/usr/lib/x.git".into())));
        assert_eq!(plan(&s, Path::new("/opt/macrofiche/git-2.55.0/r.git"), &[]), Err(JailError::Overlap("/opt/macrofiche/git-2.55.0/r.git".into())));
        assert_eq!(plan(&s, Path::new("/opt"), &[]), Err(JailError::Overlap("/opt".into())));
        let mut bad = settings();
        bad.system_dirs.push("lib".into());
        assert!(matches!(plan(&bad, Path::new("/srv/r.git"), &[]), Err(JailError::Path(_))));
        let extra: Vec<PathBuf> = (0..RULES_MAX).map(|i| PathBuf::from(format!("/srv/spool/{i}"))).collect();
        assert!(matches!(plan(&s, Path::new("/srv/r.git"), &extra), Err(JailError::TooManyRules(_))));
    }

    // Goal: the rights an ABI handles grow with it, and the repository's
    // rights exist from ABI 1 but for REFER and TRUNCATE.
    #[test]
    fn rights_by_abi() {
        assert_eq!(access::handled(1), (1 << 13) - 1);
        assert_eq!(access::handled(2) & access::REFER, access::REFER);
        assert_eq!(access::handled(4) & access::IOCTL_DEV, 0);
        assert_eq!(access::handled(5) & access::IOCTL_DEV, access::IOCTL_DEV);
        assert_eq!(access::REPO & !access::handled(3), 0);
    }

    // Goal: limits below what git needs are refused.
    #[test]
    fn limits_are_checked() {
        Limits::OPERATION.validate().unwrap();
        Limits::MAINTENANCE.validate().unwrap();
        assert!(Limits { cpu_s: 0, ..Limits::OPERATION }.validate().is_err());
        assert!(Limits { data_bytes: 1 << 20, ..Limits::OPERATION }.validate().is_err());
    }
}
