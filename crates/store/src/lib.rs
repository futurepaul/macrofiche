//! macrofiche's repositories on disk (docs/design.md): the registry, one
//! handle per repository with its write lock, the shared caches, boot
//! recovery, and idle maintenance. The pure pieces (the last-commit
//! records and rule, the LRU, the registry's pages, recovery's sweep) are
//! tested on any host; the rest needs the engine.

pub mod error;
pub mod lastcommit;
pub mod lru;
pub mod recovery;
pub mod registry;
pub mod repo;

use std::collections::HashMap;
use std::path::PathBuf;
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

impl Store {
    /// Opens the state directory: the engine checked, the registry read,
    /// and everything a killed writer left cleared, before any request.
    pub async fn open(state: PathBuf, settings: Settings) -> Result<(Arc<Store>, Recovery), StoreError> {
        let layout = Layout { state };
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
    /// twice is a 409.
    pub async fn create_repo(&self, org: &str, name: &str, default_branch: &str, now_ms: i64) -> Result<RepoMeta, StoreError> {
        if !names::valid_org(org) || !names::valid_repo_name(name) {
            return Err(StoreError::invalid("invalid_repo_name", format!("invalid repository name {name:?}")));
        }
        if !names::valid_branch(default_branch) {
            return Err(StoreError::invalid("invalid_branch", format!("invalid default_branch {default_branch:?}")));
        }
        if !self.registry.lock().expect("the registry lock").begin(org, name) {
            return Err(StoreError::conflict("repository_exists", "repository already exists"));
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
            let meta = RepoMeta { repo_id: id.clone(), repo_name: name.into(), org: org.into(), default_branch: default_branch.into(), created_at_ms: now_ms };
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

    pub fn repo_meta(&self, org: &str, name: &str) -> Option<RepoMeta> {
        self.registry.lock().expect("the registry lock").by_name(org, name).cloned()
    }

    /// The repository `name` of `org`: one handle per repository while any
    /// is alive, so its write lock is the only one.
    pub async fn repo(&self, org: &str, name: &str) -> Result<Arc<RepoHandle>, StoreError> {
        let meta = self.repo_meta(org, name).ok_or_else(|| StoreError::not_found("repository_not_found", "repository not found"))?;
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

