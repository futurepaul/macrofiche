//! The suite's fixture: the real `macrofiche` binary on a temporary state
//! directory, an org key, and calls shaped as fragment's clients shape
//! them (`cell/src/cs.rs`, `cli/src/codestorage.rs`, and the encoder of
//! `fragment_core::codestorage::commit_pack`, ported here).
#![allow(dead_code, unused_imports)]

use std::path::{Path, PathBuf};

use macrofiche_testkit::{request, Answer, Key, Macrofiche, Send, Sink};
use serde_json::{json, Value};

pub const ORG: &str = "fragment-dev";
/// The cell's own tokens live 300 s; storage tokens 900 s.
pub const TTL: i64 = 300;
/// Decoded bytes per `blob_chunk`, as fragment's encoder splits them.
pub const CHUNK_MAX: usize = macrofiche_testkit::fragment::CHUNK_MAX;

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_macrofiche"))
}

pub struct Fx {
    pub m: Macrofiche,
    pub key: Key,
    pub dir: PathBuf,
}

/// A config as fragment's dev stack writes one, and `extra` merged in.
pub fn config(dir: &Path, key: &Key, extra: Value) -> Value {
    let public = dir.join(format!("{ORG}.pub.pem"));
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&public, key.public_pem()).unwrap();
    let mut c = json!({ "orgs": [{ "name": ORG, "keys": [{ "pem_file": public }] }] });
    if let Value::Object(e) = extra {
        for (k, v) in e {
            c[k] = v;
        }
    }
    c
}

impl Fx {
    pub fn start(name: &str) -> Fx {
        Fx::start_with(name, json!({}))
    }

    pub fn start_with(name: &str, extra: Value) -> Fx {
        let dir = macrofiche_testkit::scratch(name);
        let key = Key::generate();
        let m = Macrofiche::start(&bin(), &dir, config(&dir, &key, extra));
        Fx { m, key, dir }
    }

    pub fn restart(self) -> Fx {
        let Fx { m, key, dir } = self;
        m.kill9();
        let m = Macrofiche::restart(&bin(), &dir);
        Fx { m, key, dir }
    }

    pub fn token(&self, repo: &str, scopes: &[&str]) -> String {
        self.key.token(ORG, "fragment-runtime", repo, scopes, TTL)
    }

    /// A call with a token for `repo` holding `scopes`.
    pub fn call(&self, method: &str, path: &str, repo: &str, scopes: &[&str], body: Option<(&str, &[u8])>) -> Answer {
        let auth = format!("Bearer {}", self.token(repo, scopes));
        let mut headers = vec![("authorization", auth.as_str())];
        let send = match body {
            Some((ct, b)) => {
                headers.push(("content-type", ct));
                Send::Bytes(b)
            }
            None => Send::None,
        };
        request(&self.m.url, method, path, &headers, send, Sink::Keep).expect("an answer")
    }

    pub fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Send<'_>) -> Answer {
        request(&self.m.url, method, path, headers, body, Sink::Keep).expect("an answer")
    }

    // ---- 5.1 to 5.3, as the cell's ensure_repo calls them ----

    pub fn create(&self, name: &str) -> Answer {
        let body = json!({ "repo_name": name, "default_branch": "main" }).to_string();
        self.call("POST", "/api/repos", name, &["repo:write"], Some(("application/json", body.as_bytes())))
    }

    /// The cell's whole create: the id, then its url form.
    pub fn create_repo(&self, name: &str) -> String {
        let a = self.create(name);
        assert!(matches!(a.status, 200 | 201), "create {name}: {a}");
        let id = a.json()["repo_id"].as_str().unwrap().to_string();
        let u = self.call("GET", &format!("/api/repo-urls/{id}"), &id, &["org:read"], None);
        assert_eq!(u.status, 200, "{u}");
        u.json()["url"].as_str().unwrap().to_string()
    }

    // ---- repo routes ----

    pub fn repo_path(repo: &str, op: &str) -> String {
        format!("/api/repos/{}/{op}", enc(repo).replace('/', "%2F"))
    }

    pub fn get(&self, repo: &str, op: &str, query: &str) -> Answer {
        self.call("GET", &format!("{}?{query}", Fx::repo_path(repo, op)), repo, &["git:read"], None)
    }

    pub fn head(&self, repo: &str, branch: &str) -> Option<String> {
        let a = self.get(repo, "branch", &format!("name={}", enc(branch)));
        match a.status {
            200 => Some(a.json()["branch"]["head_sha"].as_str().unwrap().to_string()),
            404 => None,
            _ => panic!("GET branch: {a}"),
        }
    }

    pub fn file(&self, repo: &str, path: &str, at: &str) -> Answer {
        self.get(repo, "file", &format!("path={}&ref={}", enc(path), enc(at)))
    }

    pub fn read(&self, repo: &str, path: &str, at: &str) -> Option<Vec<u8>> {
        let a = self.file(repo, path, at);
        match a.status {
            200 => Some(a.body),
            404 => None,
            _ => panic!("GET file: {a}"),
        }
    }

    /// A listing walked to its end, `limit` a page: (path, size, last commit).
    pub fn list(&self, repo: &str, at: &str, limit: usize) -> Vec<(String, u64, String)> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1000 {
            let mut q = format!("ref={}&limit={limit}", enc(at));
            if let Some(c) = &cursor {
                q.push_str(&format!("&cursor={}", enc(c)));
            }
            let a = self.get(repo, "files/metadata", &q);
            assert_eq!(a.status, 200, "{a}");
            let v = a.json();
            for f in v["files"].as_array().unwrap() {
                out.push((f["path"].as_str().unwrap().to_string(), f["size"].as_u64().unwrap(), f["last_commit_sha"].as_str().unwrap().to_string()));
            }
            if v["has_more"] == true {
                cursor = Some(v["next_cursor"].as_str().unwrap().to_string());
            } else {
                return out;
            }
        }
        panic!("a listing past 1000 pages")
    }

    pub fn history(&self, repo: &str, at: &str, limit: usize) -> Vec<Value> {
        let a = self.get(repo, "commits", &format!("ref={}&limit={limit}", enc(at)));
        assert_eq!(a.status, 200, "{a}");
        a.json()["commits"].as_array().unwrap().clone()
    }

    pub fn shas(&self, repo: &str, at: &str, limit: usize) -> Vec<String> {
        self.history(repo, at, limit).iter().map(|c| c["sha"].as_str().unwrap().to_string()).collect()
    }

    pub fn pack(&self, repo: &str, body: &[u8]) -> Answer {
        self.call("POST", &Fx::repo_path(repo, "commit-pack"), repo, &["git:write"], Some(("application/x-ndjson", body)))
    }

    /// One commit on main as fragment's clients make it; its answer.
    pub fn commit(&self, repo: &str, expected: Option<&str>, changes: &[Change<'_>]) -> Answer {
        self.pack(repo, &commit_pack("main", expected, "a commit", ("fragment/1a2b3c4d", "1a2b3c4d@users.fragment"), changes))
    }

    /// One commit on main at its current head; the new head.
    pub fn commit_at_head(&self, repo: &str, changes: &[Change<'_>]) -> String {
        let head = self.head(repo, "main");
        let a = self.commit(repo, head.as_deref(), changes);
        assert_eq!(a.status, 201, "{a}");
        committed(&a.json()).expect("a new sha")
    }

    pub fn post_json(&self, repo: &str, op: &str, body: &Value) -> Answer {
        let b = body.to_string();
        self.call("POST", &Fx::repo_path(repo, op), repo, &["git:write"], Some(("application/json", b.as_bytes())))
    }

    pub fn create_branch(&self, repo: &str, base: &str, target: &str, ephemeral: bool) -> Answer {
        self.post_json(repo, "branches/create", &json!({ "base_ref": base, "target_branch": target, "target_is_ephemeral": ephemeral }))
    }

    /// `merge` as fragment's deploy calls it: main into live, ff_prefer.
    pub fn merge_live(&self, repo: &str, expected_live: &str, message: &str) -> Answer {
        self.post_json(
            repo,
            "merge",
            &json!({ "target_branch": "live", "source_ref": "main", "strategy": "ff_prefer", "expected_target_sha": expected_live, "commit_message": message, "author": { "name": "fragment/1a2b3c4d", "email": "1a2b3c4d@users.fragment" } }),
        )
    }

    pub fn restore_live(&self, repo: &str, base: &str, expected_live: &str, message: &str) -> Answer {
        let line = json!({ "metadata": { "target_branch": "live", "base_ref": base, "expected_target_sha": expected_live, "commit_message": message, "author": { "name": "fragment/1a2b3c4d", "email": "1a2b3c4d@users.fragment" } } });
        self.call("POST", &Fx::repo_path(repo, "restore-commit"), repo, &["git:write"], Some(("application/x-ndjson", format!("{line}\n").as_bytes())))
    }

    pub fn preview(&self, repo: &str) -> Answer {
        self.get(repo, "merge/preview", "source_branch=main&target_branch=live")
    }

    /// `git fsck --full --strict` of every repository in the state.
    pub fn fsck_all(&self) -> Vec<(PathBuf, bool, String)> {
        fsck_all(&self.dir.join("state"))
    }
}

pub fn fsck_all(state: &Path) -> Vec<(PathBuf, bool, String)> {
    let mut out = Vec::new();
    let git = macrofiche_testkit::git_prefix().join("bin/git");
    for org in std::fs::read_dir(state.join("repos")).unwrap().flatten() {
        for repo in std::fs::read_dir(org.path()).unwrap().flatten() {
            let o = std::process::Command::new(&git)
                .env_clear()
                .env("GIT_DIR", repo.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["fsck", "--full", "--strict", "--no-progress", "--no-dangling"])
                .output()
                .unwrap();
            out.push((repo.path(), o.status.success(), String::from_utf8_lossy(&o.stderr).into_owned()));
        }
    }
    out
}

pub use macrofiche_testkit::fragment::{commit_pack, committed, enc, is_sha, up, Change};
