//! macrofiche's repositories on disk (docs/design.md): the registry, one
//! handle per repository with its write lock, the shared caches, boot
//! recovery, idle maintenance, and deletes. The pure pieces (the last-commit
//! records and rule, the LRU, the registry's pages, recovery's sweep) are
//! tested on any host; the rest needs the engine.

pub mod error;
pub mod lastcommit;
pub mod lru;
pub mod recovery;
pub mod registry;
pub mod repo;
pub mod stream;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use macrofiche_contract::names;
use macrofiche_git::jail::Settings;
use macrofiche_git::{Engine, RefOp};
use serde::Serialize;

pub use error::StoreError;
pub use recovery::Recovery;
pub use registry::{Layout, RepoMeta};
pub use repo::RepoHandle;

use registry::Registry;
use repo::Caches;

/// Handles kept open while unused (each holds a Landlock ruleset and its
/// readers); one in use stays open however many there are.
pub const HANDLES_KEPT: u64 = 256;
/// A repository idle this long after a write gets maintenance.
pub const MAINTENANCE_IDLE: Duration = Duration::from_secs(60);
/// And once a day, a gc that prunes what no ref reaches.
pub const GC_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The prune expiry: far longer than any write.
pub const PRUNE_EXPIRE: &str = "2.weeks.ago";
/// The tasks of idle maintenance.
pub const MAINTENANCE_TASKS: &[&str] = &["geometric-repack", "loose-objects", "pack-refs", "commit-graph"];

pub struct Store {
    /// The state directory's lock, held while this store is open: one
    /// process at a time serves or imports into a state directory.
    _lock: std::fs::File,
    layout: Layout,
    engine: Engine,
    registry: Mutex<Registry>,
    handles: Mutex<HashMap<String, Weak<RepoHandle>>>,
    kept: Mutex<lru::Lru<String, Arc<RepoHandle>>>,
    caches: Arc<Caches>,
    rng: ring::rand::SystemRandom,
    gc_at: Mutex<HashMap<String, Instant>>,
    dirty: repo::Dirty,
}

/// One repository's maintenance, as it went.
#[derive(Clone, Debug, Serialize)]
pub struct Maintained {
    pub org: String,
    pub repo_id: String,
    pub ms: u128,
    pub gc: bool,
    pub ok: bool,
    pub detail: String,
}

fn key(org: &str, id: &str) -> String {
    format!("{org}/{id}")
}

/// An id an import keeps (the store it came from chose it) is at most this.
pub const REPO_ID_BYTES_MAX: usize = 64;

/// One repository to import (`Store::import_repo`).
pub struct Import<'a> {
    pub org: &'a str,
    pub name: &'a str,
    /// The url form its clients call it by, when not its name.
    pub url: Option<&'a str>,
    /// The id it had, kept; else a new one.
    pub repo_id: Option<&'a str>,
    pub default_branch: &'a str,
    pub created_at_ms: i64,
    /// A fast-import stream, ending in `done`.
    pub stream: &'a Path,
}

/// What an import made.
#[derive(Clone, Debug, Serialize)]
pub struct Imported {
    pub meta: RepoMeta,
    /// Each branch, at its commit.
    pub branches: Vec<(String, String)>,
    /// The legacy commit ids it answers to (`refs/macrofiche/legacy/`).
    pub legacy: usize,
}

/// Takes the state directory's lock, exclusively and at once: refused
/// while another process holds it (it serves, or imports, there). A POSIX
/// record lock: held by this process, never by a child it forks (git's
/// processes), and let go by the kernel however the process ends. It does
/// not tell two opens in one process apart (only tests make those).
fn lock_state(path: &Path) -> Result<std::fs::File, StoreError> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: a zeroed `flock` is a valid value; its fields are set below.
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = libc::F_WRLCK as libc::c_short;
    l.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: a valid descriptor this function owns, and a valid `flock`.
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &l) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EAGAIN | libc::EACCES) => StoreError::Unavailable(format!("{} is held: another macrofiche serves or imports into this state directory", path.display())),
            _ => e.into(),
        });
    }
    Ok(f)
}

/// What a delete did (contract 5.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deleted {
    /// Deleted by this call: its identity, marked.
    Now(RepoMeta),
    /// Deleted before it.
    Already,
}

/// Removes everything a deleted repository held but its marked
/// `repo.json`: its git data, its records, its webhooks. A delete runs it,
/// and boot runs it again for one a crash cut short; what is gone already
/// is no error.
fn purge(dir: &Path) -> std::io::Result<()> {
    let remove = |e: std::fs::DirEntry| -> std::io::Result<()> {
        match e.file_type()?.is_dir() {
            true => std::fs::remove_dir_all(e.path()),
            false => std::fs::remove_file(e.path()),
        }
    };
    let meta = Path::new(registry::META_FILE);
    let (meta_dir, meta_file) = (meta.parent().expect("repo.json is in a directory"), meta.file_name().expect("repo.json is a file"));
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_name() != meta_dir.as_os_str() {
            remove(e)?;
            continue;
        }
        for f in std::fs::read_dir(e.path())? {
            let f = f?;
            if f.file_name() != meta_file {
                remove(f)?;
            }
        }
    }
    registry::sync_dir(dir)
}

impl Store {
    /// Opens the state directory: the engine checked, the registry read,
    /// and everything a killed writer left cleared, before any request.
    pub async fn open(state: PathBuf, settings: Settings) -> Result<(Arc<Store>, Recovery), StoreError> {
        let layout = Layout { state };
        // before anything is cleared: another process's spools and half-made
        // repositories are its own, never this one's to sweep
        std::fs::create_dir_all(&layout.state)?;
        let lock = lock_state(&layout.lock())?;
        let mut rec = Recovery::default();
        for d in [layout.tmp(), layout.spool()] {
            if d.exists() {
                for e in std::fs::read_dir(&d)? {
                    let p = e?.path();
                    if p.is_dir() {
                        std::fs::remove_dir_all(&p)?;
                    } else {
                        std::fs::remove_file(&p)?;
                    }
                    if d == layout.tmp() {
                        rec.half_made += 1;
                    } else {
                        rec.spools += 1;
                    }
                }
            }
        }
        for d in [layout.repos(), layout.tmp(), layout.spool(), layout.scratch()] {
            std::fs::create_dir_all(&d)?;
        }
        let engine = Engine::open(settings, &layout.scratch()).await?;
        let (registry, strays) = Registry::load(&layout)?;
        rec.strays = strays.len();
        let store = Arc::new(Store {
            _lock: lock,
            layout,
            engine,
            registry: Mutex::new(Registry::default()),
            handles: Mutex::new(HashMap::new()),
            kept: Mutex::new(lru::Lru::new(HANDLES_KEPT)),
            caches: Arc::new(Caches::default()),
            rng: ring::rand::SystemRandom::new(),
            gc_at: Mutex::new(HashMap::new()),
            dirty: Arc::new(Mutex::new(HashMap::new())),
        });
        let metas: Vec<RepoMeta> = {
            let mut reg = store.registry.lock().expect("the registry lock");
            *reg = registry;
            reg.all()
        };
        for m in metas {
            let dir = store.layout.repo(&m.org, &m.repo_id);
            if m.deleted_at_ms.is_some() {
                // a delete a crash cut short ends here; done, it is a no-op
                purge(&dir)?;
                rec.deleted += 1;
                continue;
            }
            recovery::sweep_repo(&dir, &mut rec)?;
            let git = store.engine.repo(&dir)?;
            for (name, oid) in git.refs("refs/macrofiche/tmp/").await? {
                git.update_refs(&[RefOp::Delete { name, old: oid }]).await?.map_err(|r| StoreError::Internal(format!("a temporary ref: {r:?}")))?;
                rec.temp_refs += 1;
            }
        }
        Ok((store, rec))
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn random_id(&self) -> Result<String, StoreError> {
        let mut b = [0u8; 16];
        ring::rand::SecureRandom::fill(&self.rng, &mut b).map_err(|_| StoreError::Internal("no randomness".into()))?;
        Ok(b.iter().map(|x| format!("{x:02x}")).collect())
    }

    /// A random name for a request's spool directory.
    pub fn spool_dir(&self) -> Result<PathBuf, StoreError> {
        Ok(self.layout.spool().join(self.random_id()?))
    }

    /// Creates a repository (contract 5.1): made in `tmp/`, initialized,
    /// its identity written, then renamed into place whole. A name made
    /// twice is a 409, and so is a deleted one's (`repository_deleted`).
    pub async fn create_repo(&self, org: &str, name: &str, default_branch: &str, now_ms: i64) -> Result<RepoMeta, StoreError> {
        if !names::valid_org(org) || !names::valid_repo_name(name) {
            return Err(StoreError::invalid("invalid_repo_name", format!("invalid repository name {name:?}")));
        }
        if !names::valid_branch(default_branch) {
            return Err(StoreError::invalid("invalid_branch", format!("invalid default_branch {default_branch:?}")));
        }
        {
            let mut reg = self.registry.lock().expect("the registry lock");
            if !reg.begin(org, name) {
                if reg.entry(org, name).is_some_and(|m| m.deleted_at_ms.is_some()) {
                    return Err(StoreError::conflict("repository_deleted", "repository deleted: its name is not made again"));
                }
                return Err(StoreError::conflict("repository_exists", "repository already exists"));
            }
        }
        let made = self.make_repo(org, name, default_branch, now_ms).await;
        let meta = made.as_ref().ok().cloned();
        self.registry.lock().expect("the registry lock").end(org, name, meta);
        made
    }

    async fn make_repo(&self, org: &str, name: &str, default_branch: &str, now_ms: i64) -> Result<RepoMeta, StoreError> {
        let id = self.random_id()?;
        let tmp = self.layout.tmp().join(format!("{id}.git"));
        std::fs::create_dir_all(&tmp)?;
        let result = async {
            self.engine.init(&tmp, default_branch).await?;
            let meta = RepoMeta { repo_id: id.clone(), repo_name: name.into(), org: org.into(), default_branch: default_branch.into(), created_at_ms: now_ms, deleted_at_ms: None, url: None };
            let bytes = serde_json::to_vec_pretty(&meta).expect("a meta serializes");
            repo::write_durably(&tmp.join(registry::META_FILE), &bytes)?;
            std::fs::create_dir_all(self.layout.org(org))?;
            registry::rename_noreplace(&tmp, &self.layout.repo(org, &id))?;
            Ok::<_, StoreError>(meta)
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        result
    }

    /// Deletes the repository whose url form is `url` (contract 5.13),
    /// under its write lock so no write is halfway through it: marked
    /// deleted in its `repo.json` first, durably, then everything else it
    /// held removed. From then it serves nothing (a write that waited on
    /// the lock is refused), leaves the org's list, and its name is never
    /// made again.
    pub async fn delete_repo(&self, org: &str, url: &str, now_ms: i64) -> Result<Deleted, StoreError> {
        let meta = match self.registry.lock().expect("the registry lock").entry_by_url(org, url).cloned() {
            None => return Err(StoreError::not_found("repository_not_found", "repository not found")),
            Some(m) if m.deleted_at_ms.is_some() => return Ok(Deleted::Already),
            Some(m) => m,
        };
        let h = self.handle(meta.clone())?;
        // refused only when another delete ran while this one waited
        let Ok(_w) = h.write().await else { return Ok(Deleted::Already) };
        let marked = RepoMeta { deleted_at_ms: Some(now_ms), ..meta };
        repo::write_durably(&h.dir.join(registry::META_FILE), &serde_json::to_vec_pretty(&marked).expect("a meta serializes"))?;
        h.gone();
        self.registry.lock().expect("the registry lock").mark_deleted(org, &marked.repo_name, now_ms);
        self.dirty.lock().expect("the dirty lock").remove(&key(org, &marked.repo_id));
        purge(&h.dir)?;
        Ok(Deleted::Now(marked))
    }

    /// Imports a repository made elsewhere (docs/design.md, Import): its
    /// whole history from `stream`, a fast-import stream, into a new
    /// repository named `name`, made in `tmp/` and renamed into place whole
    /// as a create is. It may keep the url form and id it was known by
    /// (`url`, `repo_id`), each refused when taken. The stream may write
    /// branches (`refs/heads/`, the ephemeral namespace) and the legacy ids
    /// of its commits (`refs/macrofiche/legacy/<40 hex>`, each a commit);
    /// any other ref, or a repository `fsck --strict` faults, is refused
    /// and nothing is kept.
    pub async fn import_repo(&self, req: &Import<'_>) -> Result<Imported, StoreError> {
        let (org, name) = (req.org, req.name);
        if !names::valid_org(org) || !names::valid_repo_name(name) {
            return Err(StoreError::invalid("invalid_repo_name", format!("invalid repository name {name:?}")));
        }
        if req.url.is_some_and(|u| !names::valid_repo_name(u)) {
            return Err(StoreError::invalid("invalid_url", format!("invalid url form {:?}", req.url)));
        }
        if req.repo_id.is_some_and(|id| id.is_empty() || id.len() > REPO_ID_BYTES_MAX || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')) {
            return Err(StoreError::invalid("invalid_repo_id", format!("a repo id is 1 to {REPO_ID_BYTES_MAX} letters, digits, _ and -, not {:?}", req.repo_id)));
        }
        if !names::valid_branch(req.default_branch) {
            return Err(StoreError::invalid("invalid_branch", format!("invalid default_branch {:?}", req.default_branch)));
        }
        let id = match req.repo_id {
            Some(id) => id.to_string(),
            None => self.random_id()?,
        };
        if !self.registry.lock().expect("the registry lock").begin_import(org, name, req.url, &id) {
            return Err(StoreError::conflict("repository_exists", format!("{name}, its url form or its id is taken")));
        }
        let made = self.make_imported(req, &id).await;
        let meta = made.as_ref().ok().map(|i| i.meta.clone());
        self.registry.lock().expect("the registry lock").end(org, name, meta);
        made
    }

    async fn make_imported(&self, req: &Import<'_>, id: &str) -> Result<Imported, StoreError> {
        // what the stream writes, read before git sees it: names git takes
        // that an import must not (a root ref, `shallow`) are refused here
        stream::check(std::fs::File::open(req.stream)?).map_err(|r| StoreError::invalid("invalid_stream", format!("line {}: {}", r.line, r.why)))?;
        let tmp = self.layout.tmp().join(format!("{id}.git"));
        std::fs::create_dir_all(&tmp)?;
        let result = async {
            let git = self.engine.init(&tmp, req.default_branch).await?;
            git.import_stream(req.stream).await.map_err(|e| match e {
                macrofiche_git::GitError::Failed { stderr, .. } => StoreError::invalid("invalid_stream", format!("fast-import refused the stream: {}", stderr.trim())),
                e => e.into(),
            })?;
            let mut branches = vec![];
            let mut legacy = 0usize;
            let refused = |why: String| StoreError::invalid("invalid_stream", why);
            // every ref, the root refs too (`ORIG_HEAD` and the like, which
            // a listing of `refs/` never shows); HEAD alone is checked below
            let listed = git.run("for-each-ref", &["for-each-ref", "--include-root-refs", "--format=%(objectname) %(refname)"], macrofiche_git::RunOpts { stdout_max: 64 << 20, ..macrofiche_git::RunOpts::read() }).await?.ok("for-each-ref")?;
            let all = macrofiche_git::parse::refs(&listed, macrofiche_git::linux::plumbing::REFS_MAX).map_err(|e| StoreError::Internal(e.to_string()))?;
            let git = &git;
            let cat = |what: String| async move { git.run("cat-file", &["cat-file", "-t", &what], macrofiche_git::RunOpts::read()).await };
            for (name, oid) in all {
                if name == "HEAD" {
                    continue;
                }
                let branch = name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix(repo::EPHEMERAL_PREFIX));
                let old = name.strip_prefix(repo::LEGACY_PREFIX);
                match (branch, old) {
                    (Some(b), None) if names::valid_branch(b) => branches.push((name.clone(), oid.to_string())),
                    (None, Some(old)) if names::is_sha(old) => {
                        // an alias is for an id that is no object here: one
                        // naming a real commit would let a write prepared
                        // against that commit pass for another's
                        if cat(old.to_string()).await?.success() {
                            return Err(refused(format!("{name} names an id this repository holds as an object: a legacy id is only an id it does not")));
                        }
                        legacy += 1;
                    }
                    _ => return Err(refused(format!("the stream wrote {name}: an import writes branches and legacy ids alone"))),
                }
                let is_commit = cat(oid.to_string()).await?.ok("cat-file")?;
                if String::from_utf8_lossy(&is_commit).trim() != "commit" {
                    return Err(refused(format!("{name} names no commit")));
                }
            }
            // and what git left, as a backstop: nothing beside what an
            // initialized repository and its objects and refs are
            for e in std::fs::read_dir(&tmp)? {
                let name = e?.file_name();
                let name = name.to_string_lossy();
                if !matches!(name.as_ref(), "HEAD" | "config" | "objects" | "refs" | "packed-refs" | "macrofiche") {
                    return Err(refused(format!("the stream left {name} in the repository: an import writes branches and legacy ids alone")));
                }
            }
            if std::fs::read_dir(tmp.join("objects/info"))?.next().is_some() {
                return Err(refused("the stream left files in objects/info".into()));
            }
            let head = git.run("symbolic-ref", &["symbolic-ref", "HEAD"], macrofiche_git::RunOpts::read()).await?.ok("symbolic-ref")?;
            if String::from_utf8_lossy(&head).trim() != format!("refs/heads/{}", req.default_branch) {
                return Err(refused(format!("HEAD is {}, not the default branch", String::from_utf8_lossy(&head).trim())));
            }
            let fsck = git.fsck().await?;
            if !fsck.ok {
                return Err(StoreError::invalid("invalid_stream", format!("fsck --strict: {}", fsck.stderr.trim())));
            }
            let meta = RepoMeta {
                repo_id: id.to_string(),
                repo_name: req.name.into(),
                org: req.org.into(),
                default_branch: req.default_branch.into(),
                created_at_ms: req.created_at_ms,
                deleted_at_ms: None,
                url: req.url.map(str::to_string),
            };
            repo::write_durably(&tmp.join(registry::META_FILE), &serde_json::to_vec_pretty(&meta).expect("a meta serializes"))?;
            std::fs::create_dir_all(self.layout.org(req.org))?;
            registry::rename_noreplace(&tmp, &self.layout.repo(req.org, id))?;
            Ok::<_, StoreError>(Imported { meta, branches, legacy })
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        result
    }

    pub fn repos_page(&self, org: &str, after: Option<(i64, &str)>, limit: usize, q: Option<&str>) -> (Vec<RepoMeta>, bool) {
        self.registry.lock().expect("the registry lock").page(org, after, limit, q)
    }

    /// Ends readers idle past their time in every open repository.
    pub fn reap_idle(&self) -> usize {
        let open: Vec<Arc<RepoHandle>> = self.handles.lock().expect("the handles lock").values().filter_map(Weak::upgrade).collect();
        open.iter().map(|h| h.pool.reap()).sum()
    }

    pub fn repo_by_id(&self, org: &str, id: &str) -> Option<RepoMeta> {
        self.registry.lock().expect("the registry lock").by_id(org, id).cloned()
    }

    /// The repository whose url form is `url`, unless it was deleted.
    pub fn repo_meta(&self, org: &str, url: &str) -> Option<RepoMeta> {
        self.registry.lock().expect("the registry lock").by_url(org, url).cloned()
    }

    /// The repository of `org` whose url form is `url`, as every
    /// repo-scoped call names it: one handle per repository while any is
    /// alive, so its write lock is the only one.
    pub async fn repo(&self, org: &str, url: &str) -> Result<Arc<RepoHandle>, StoreError> {
        let meta = self.repo_meta(org, url).ok_or_else(|| StoreError::not_found("repository_not_found", "repository not found"))?;
        self.handle(meta)
    }

    fn handle(&self, meta: RepoMeta) -> Result<Arc<RepoHandle>, StoreError> {
        let k = key(&meta.org, &meta.repo_id);
        let mut handles = self.handles.lock().expect("the handles lock");
        if let Some(h) = handles.get(&k).and_then(Weak::upgrade) {
            drop(handles);
            self.kept.lock().expect("the kept lock").insert(k, h.clone(), 1);
            return Ok(h);
        }
        let dir = self.layout.repo(&meta.org, &meta.repo_id);
        let git = self.engine.repo(&dir)?;
        let h = Arc::new(RepoHandle::new(meta, dir, git, self.caches.clone(), self.dirty.clone()));
        if handles.len() as u64 > 4 * HANDLES_KEPT {
            handles.retain(|_, w| w.strong_count() > 0);
        }
        handles.insert(k.clone(), Arc::downgrade(&h));
        drop(handles);
        self.kept.lock().expect("the kept lock").insert(k, h.clone(), 1);
        Ok(h)
    }

    /// One pass of idle maintenance: every repository written to and idle
    /// for `idle` gets git's maintenance tasks under its write lock (never
    /// beside a writer), and a gc once a day.
    pub async fn maintain(&self, idle: Duration) -> Vec<Maintained> {
        let due: Vec<(String, RepoMeta, Instant)> = {
            let dirty = self.dirty.lock().expect("the dirty lock");
            dirty.iter().filter(|(_, (_, t))| t.elapsed() >= idle).map(|(k, (m, t))| (k.clone(), m.clone(), *t)).collect()
        };
        let mut out = Vec::new();
        for (k, meta, written) in due {
            if self.repo_by_id(&meta.org, &meta.repo_id).is_none() {
                // deleted since it was written
                continue;
            }
            let Ok(h) = self.handle(meta) else { continue };
            let Ok(_w) = h.write_lock.try_lock() else { continue };
            let t0 = Instant::now();
            let gc = self.gc_at.lock().expect("the gc lock").get(&k).is_none_or(|t| t.elapsed() >= GC_EVERY);
            let mut r = h.git.maintenance(MAINTENANCE_TASKS).await;
            if gc && r.as_ref().is_ok_and(|o| o.success()) {
                r = h.git.gc(PRUNE_EXPIRE).await;
                if r.as_ref().is_ok_and(|o| o.success()) {
                    self.gc_at.lock().expect("the gc lock").insert(k.clone(), Instant::now());
                }
            }
            let (ok, detail) = match r {
                Ok(o) if o.success() => (true, String::new()),
                Ok(o) => (false, o.stderr),
                Err(e) => (false, e.to_string()),
            };
            if ok {
                // Unless it was written again meanwhile (it was not: the
                // lock was held), it is clean.
                let mut dirty = self.dirty.lock().expect("the dirty lock");
                if dirty.get(&k).is_some_and(|(_, t)| *t == written) {
                    dirty.remove(&k);
                }
            }
            out.push(Maintained { org: h.meta.org.clone(), repo_id: h.meta.repo_id.clone(), ms: t0.elapsed().as_millis(), gc, ok, detail });
        }
        out
    }

    /// Repositories waiting for maintenance.
    pub fn dirty(&self) -> usize {
        self.dirty.lock().expect("the dirty lock").len()
    }
}

