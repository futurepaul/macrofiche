//! Jailed git processes: spawned by absolute path with the environment
//! cleared, each its own process group, limited, restricted between its
//! fork and its exec (no allocation there: the Landlock ruleset and the
//! seccomp program are built before the fork), killed as a group when
//! its deadline passes or its owner drops it.

use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::landlock::{self, Ruleset};
use super::seccomp;
use crate::error::{GitError, STDERR_KEPT_BYTES};
use crate::jail::{self, Limits, Settings};
use crate::pin;

/// git processes alive at once, across every repository.
pub const PROCESSES_MAX: usize = 256;
/// How long a spawn waits for a process slot before it fails.
pub const SLOT_WAIT: Duration = Duration::from_secs(30);

/// The identity git falls back to when a call names none (reflog-free
/// ref updates, maintenance): never the node's user.
pub const DEFAULT_NAME: &str = "macrofiche";
pub const DEFAULT_EMAIL: &str = "macrofiche@macrofiche.invalid";

struct Inner {
    settings: Settings,
    git: PathBuf,
    exec_path: PathBuf,
    path_env: PathBuf,
    seccomp: Arc<Vec<libc::sock_filter>>,
    slots: Arc<Semaphore>,
    landlock_abi: u32,
}

/// The engine: a checked git prefix and the jail every process gets.
#[derive(Clone)]
pub struct Engine(Arc<Inner>);

impl Engine {
    /// Opens the prefix and checks it is the pinned git: `bin/git`
    /// answers `--version` with the pin's line, run jailed in `scratch`
    /// (an empty directory macrofiche owns).
    pub async fn open(settings: Settings, scratch: &Path) -> Result<Engine, GitError> {
        let prefix = settings.prefix.clone();
        let git = prefix.join("bin/git");
        let engine_err = |detail: String| GitError::Engine { prefix: prefix.display().to_string(), detail };
        if !git.is_file() {
            return Err(engine_err(format!("no {} (cargo xtask engine builds it)", git.display())));
        }
        let landlock_abi = if settings.enforce { landlock::abi() } else { 0 };
        if settings.enforce && landlock_abi < jail::LANDLOCK_ABI_MIN {
            return Err(GitError::Jail("this kernel has no Landlock; the jail cannot be applied".into()));
        }
        let engine = Engine(Arc::new(Inner {
            exec_path: prefix.join("libexec/git-core"),
            path_env: prefix.join("bin"),
            git,
            seccomp: Arc::new(seccomp::program()),
            slots: Arc::new(Semaphore::new(PROCESSES_MAX)),
            landlock_abi,
            settings,
        }));
        let repo = engine.repo(scratch)?;
        let out = repo.run("version", &["--version"], RunOpts::read()).await?;
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.success() || line != pin::VERSION_LINE {
            return Err(engine_err(format!("git --version says {line:?}, not {:?}", pin::VERSION_LINE)));
        }
        Ok(engine)
    }

    pub fn settings(&self) -> &Settings {
        &self.0.settings
    }

    /// The kernel's Landlock ABI, 0 with the jail off.
    pub fn landlock_abi(&self) -> u32 {
        self.0.landlock_abi
    }

    /// git processes running now.
    pub fn processes(&self) -> usize {
        PROCESSES_MAX - self.0.slots.available_permits()
    }

    /// A handle on the repository at `dir`, its jail built (the directory
    /// must exist: Landlock binds rules to it).
    pub fn repo(&self, dir: &Path) -> Result<Repo, GitError> {
        let plan = jail::plan(&self.0.settings, dir, &[]).map_err(|e| GitError::Jail(e.to_string()))?;
        let ruleset = if self.0.settings.enforce {
            Some(Arc::new(landlock::build(&plan).map_err(|e| GitError::Jail(e.to_string()))?))
        } else {
            None
        };
        Ok(Repo { engine: self.clone(), dir: dir.to_path_buf(), tmp: plan.tmp, ruleset })
    }
}

/// One repository's processes: its directory and its jail.
#[derive(Clone)]
pub struct Repo {
    engine: Engine,
    dir: PathBuf,
    tmp: PathBuf,
    ruleset: Option<Arc<Ruleset>>,
}

/// How to run one git: its limits, deadline, input, and output bound.
pub struct RunOpts {
    pub limits: Limits,
    pub deadline: Duration,
    pub stdin: Option<Vec<u8>>,
    /// Bytes of stdout kept before the run fails as too large.
    pub stdout_max: usize,
    /// Extra environment: the author and committer of a commit.
    pub env: Vec<(&'static str, String)>,
}

/// A read's deadline.
pub const READ_DEADLINE: Duration = Duration::from_secs(30);
/// stdout a plain call may answer.
pub const STDOUT_MAX: usize = 1 << 20;

impl RunOpts {
    pub fn read() -> RunOpts {
        RunOpts { limits: Limits::OPERATION, deadline: READ_DEADLINE, stdin: None, stdout_max: STDOUT_MAX, env: Vec::new() }
    }
}

/// A finished git.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The failure as an error, with its stderr.
    pub fn error(&self, op: &'static str) -> GitError {
        match (self.code, self.signal) {
            (_, Some(signal)) => GitError::Killed { op, signal, stderr: self.stderr.clone() },
            (code, None) => GitError::Failed { op, code: code.unwrap_or(-1), stderr: self.stderr.clone() },
        }
    }

    /// stdout, or the failure.
    pub fn ok(self, op: &'static str) -> Result<Vec<u8>, GitError> {
        if self.success() {
            Ok(self.stdout)
        } else {
            Err(self.error(op))
        }
    }
}

/// A running jailed git. Dropped before it is reaped, it takes its whole
/// process group with it.
pub struct Proc {
    pub op: &'static str,
    child: tokio::process::Child,
    pid: i32,
    _slot: OwnedSemaphorePermit,
    reaped: bool,
}

impl Proc {
    pub fn stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }
    pub fn stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }
    pub fn stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Waits for it to end.
    pub async fn wait(mut self) -> Result<std::process::ExitStatus, GitError> {
        let op = self.op;
        let status = self.child.wait().await.map_err(|source| GitError::Io { op, source })?;
        self.reaped = true;
        Ok(status)
    }

    /// Ends its group now.
    pub fn kill(&mut self) {
        if !self.reaped {
            // SAFETY: kill(2) on the process group this git leads.
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = self.child.start_kill();
        }
    }
}

/// The stdin, stdout, and stderr a spawn sets up.
#[derive(Clone, Copy)]
pub struct Pipes {
    pub stdin: bool,
    pub stdout: bool,
}

impl Repo {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Spawns `git <args>` jailed for this repository.
    pub async fn spawn(&self, op: &'static str, args: &[&str], env: &[(&'static str, String)], limits: Limits, pipes: Pipes) -> Result<Proc, GitError> {
        limits.validate().map_err(|e| GitError::Jail(e.to_string()))?;
        let slot = tokio::time::timeout(SLOT_WAIT, self.engine.0.slots.clone().acquire_owned())
            .await
            .map_err(|_| GitError::Timeout { op, after: SLOT_WAIT })?
            .expect("the slots are never closed");
        let inner = &self.engine.0;
        let mut cmd = tokio::process::Command::new(&inner.git);
        cmd.env_clear()
            .env("GIT_DIR", &self.dir)
            .env("GIT_EXEC_PATH", &inner.exec_path)
            .env("PATH", &inner.path_env)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ADVICE", "0")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("TMPDIR", &self.tmp)
            .env("GIT_AUTHOR_NAME", DEFAULT_NAME)
            .env("GIT_AUTHOR_EMAIL", DEFAULT_EMAIL)
            .env("GIT_COMMITTER_NAME", DEFAULT_NAME)
            .env("GIT_COMMITTER_EMAIL", DEFAULT_EMAIL);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(args.iter().map(OsStr::new))
            .current_dir(&self.dir)
            .process_group(0)
            .stdin(if pipes.stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(if pipes.stdout { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let ruleset = self.ruleset.as_ref().map(|r| r.raw());
        let enforce = inner.settings.enforce;
        let program = inner.seccomp.clone();
        // SAFETY: getpid has no preconditions.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the closure runs between fork and exec and only makes
        // syscalls on data prepared before the fork; it allocates nothing
        // (errors are raw OS errors).
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died between the fork and the prctl.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                for (resource, value) in [
                    (libc::RLIMIT_CORE, 0),
                    (libc::RLIMIT_CPU, limits.cpu_s),
                    (libc::RLIMIT_DATA, limits.data_bytes),
                    (libc::RLIMIT_FSIZE, limits.fsize_bytes),
                    (libc::RLIMIT_NOFILE, limits.nofile),
                ] {
                    let rl = libc::rlimit { rlim_cur: value, rlim_max: value };
                    if libc::setrlimit(resource, &rl) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if enforce {
                    if let Some(fd) = ruleset {
                        landlock::restrict_self(fd)?;
                    }
                    seccomp::install(&program)?;
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|source| GitError::Spawn { op, source })?;
        let pid = child.id().expect("a child just spawned has a pid") as i32;
        Ok(Proc { op, child, pid, _slot: slot, reaped: false })
    }

    /// Runs `git <args>` to its end: stdin written, stdout kept up to
    /// `opts.stdout_max`, stderr up to `STDERR_KEPT_BYTES`, within the
    /// deadline (the group is killed past it).
    pub async fn run(&self, op: &'static str, args: &[&str], opts: RunOpts) -> Result<Output, GitError> {
        let RunOpts { limits, deadline, stdin, stdout_max, env } = opts;
        let mut proc = self.spawn(op, args, &env, limits, Pipes { stdin: stdin.is_some(), stdout: true }).await?;
        let input = proc.stdin();
        let mut out = proc.stdout().expect("piped");
        let mut err = proc.stderr().expect("piped");
        let work = async {
            let write = async {
                if let (Some(mut w), Some(bytes)) = (input, stdin) {
                    // A git that stops reading early (and fails) closes the
                    // pipe; its exit status says why.
                    let _ = w.write_all(&bytes).await;
                    let _ = w.shutdown().await;
                }
            };
            let read_out = async {
                let mut buf = Vec::new();
                (&mut out).take(stdout_max as u64 + 1).read_to_end(&mut buf).await.map(|_| buf)
            };
            let read_err = async {
                let mut buf = Vec::new();
                let r = (&mut err).take(STDERR_KEPT_BYTES as u64).read_to_end(&mut buf).await;
                // Drain the rest so git never blocks on a full pipe.
                let _ = tokio::io::copy(&mut err, &mut tokio::io::sink()).await;
                r.map(|_| buf)
            };
            let ((), stdout, stderr) = tokio::join!(write, read_out, read_err);
            (stdout, stderr)
        };
        let (stdout, stderr) = tokio::time::timeout(deadline, work).await.map_err(|_| GitError::Timeout { op, after: deadline })?;
        let stdout = stdout.map_err(|source| GitError::Io { op, source })?;
        if stdout.len() > stdout_max {
            return Err(GitError::OutputTooLarge { op, max: stdout_max });
        }
        let stderr = String::from_utf8_lossy(&stderr.map_err(|source| GitError::Io { op, source })?).into_owned();
        let status = tokio::time::timeout(deadline, proc.wait()).await.map_err(|_| GitError::Timeout { op, after: deadline })??;
        Ok(Output { code: status.code(), signal: status.signal(), stdout, stderr })
    }
}
