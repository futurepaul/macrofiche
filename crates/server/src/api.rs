//! The HTTP API (docs/contract.md, sections 5 and 15): each route checked
//! as the service checks it (auth, scope, repo claim, then the request),
//! answered in the service's shapes. Commit packs are read as they arrive
//! into spools; files are streamed out of `cat-file`.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response};
use macrofiche_auth::{authorize, bearer, verify, AuthError, Claims, Keyring, Scope};
use macrofiche_contract::limits::*;
use macrofiche_contract::names::{self, ZERO_SHA};
use macrofiche_contract::time::{http_date, now_s, rfc3339};
use macrofiche_contract::{problem, Cursor, Problem, Query};
use macrofiche_git::{Ident, Oid};
use macrofiche_pack::{Event, Op, PackError, Parser, Shape};
use macrofiche_store::repo::{CommitOutcome, CreateOutcome, MergeOutcome, MergeRequest, Moved, PackChange, PackFile, RestoreOutcome, Strategy};
use macrofiche_store::{RepoHandle, Store, StoreError};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::config::OrgHook;
use crate::webhook::{self, Hooks};

pub type Body = BoxBody<Bytes, std::io::Error>;
pub type Resp = Response<Body>;

/// Requests served at once; past it, 503.
pub const REQUESTS_INFLIGHT_MAX: usize = 1024;
/// Commit packs uploading at once (their spools), and how long one waits
/// for a slot.
pub const PACKS_INFLIGHT_MAX: usize = 64;
pub const PACK_SLOT_WAIT: Duration = Duration::from_secs(10);
/// A body that sends nothing this long is dropped.
pub const BODY_IDLE: Duration = Duration::from_secs(60);
/// A streamed file's chunk that its client does not take this long ends
/// the stream (and its reader).
pub const SEND_IDLE: Duration = Duration::from_secs(60);
/// Chunks of a file in flight to its client.
const STREAM_DEPTH: usize = 4;

/// A body's deadline: a minute, and a second for each 16 KiB, half the
/// CLI's 32 KiB/s floor (contract section 8), so the client's own
/// deadline always ends first.
pub fn body_deadline(bytes: u64) -> Duration {
    Duration::from_secs(60 + bytes / (16 * 1024))
}

pub struct App {
    pub store: Arc<Store>,
    pub keyring: Keyring,
    pub org_hooks: HashMap<String, Vec<OrgHook>>,
    pub hooks: Hooks,
    pub requests: Arc<Semaphore>,
    pub packs: Arc<Semaphore>,
}

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|n: Infallible| match n {}).boxed()
}

fn empty() -> Body {
    full(Bytes::new())
}

fn json_resp(status: u16, v: &Value, content_type: &str) -> Resp {
    let mut r = Response::new(full(v.to_string()));
    *r.status_mut() = hyper::StatusCode::from_u16(status).expect("a status");
    r.headers_mut().insert("content-type", content_type.parse().expect("a header"));
    r
}

pub fn json(status: u16, v: &Value) -> Resp {
    json_resp(status, v, "application/json")
}

pub fn problem_resp(p: &Problem) -> Resp {
    json_resp(p.status, &p.body(), problem::CONTENT_TYPE)
}

fn header(r: &mut Resp, k: &'static str, v: &str) {
    if let Ok(v) = v.parse() {
        r.headers_mut().insert(k, v);
    }
}

fn bad(code: &'static str, detail: impl Into<String>) -> Problem {
    Problem::bad_request(code, detail)
}

fn store_err(e: StoreError) -> Problem {
    if matches!(e, StoreError::Unavailable(_) | StoreError::Internal(_)) {
        eprintln!("{}", json!({"event": "store.error", "error": e.to_string()}));
    }
    e.problem()
}

fn hex(o: Option<Oid>) -> String {
    o.map_or_else(|| ZERO_SHA.to_string(), |o| o.to_string())
}

/// Serves one request; never fails (a failure is an answer).
pub async fn handle(app: Arc<App>, req: Request<Incoming>) -> Result<Resp, Infallible> {
    let Ok(_slot) = app.requests.clone().try_acquire_owned() else {
        return Ok(problem_resp(&Problem::unavailable("macrofiche is serving as many requests as it can")));
    };
    Ok(route(&app, req).await.unwrap_or_else(|p| problem_resp(&p)))
}

fn claims(app: &App, req: &Request<Incoming>) -> Result<Claims, Problem> {
    let h = req.headers().get("authorization").and_then(|v| v.to_str().ok());
    let token = bearer(h).map_err(|e| e.problem())?;
    verify(&app.keyring, token, now_s()).map_err(|e| e.problem())
}

async fn route(app: &App, req: Request<Incoming>) -> Result<Resp, Problem> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if path == "/healthz" {
        return Ok(json(200, &json!({ "ok": true })));
    }
    if !path.starts_with("/api/") {
        if let Some((name, op)) = crate::smart::parse_path(&path) {
            return crate::smart::serve(app, req, name, op).await;
        }
    }
    let query = Query::parse(req.uri().query()).map_err(|e| bad("invalid_query", e.to_string()))?;
    if path == "/api/repos" {
        let c = claims(app, &req)?;
        return match method {
            Method::POST => create_repo(app, &c, &query, req).await,
            Method::GET => list_repos(app, &c, &query),
            _ => Err(Problem::new(405, "method_not_allowed", format!("{method} /api/repos"))),
        };
    }
    if let Some(id) = path.strip_prefix("/api/repo-urls/") {
        let c = claims(app, &req)?;
        authorize(&c, Scope::OrgRead, None).map_err(|e| e.problem())?;
        let id = macrofiche_contract::query::decode_segment(id).map_err(|e| bad("invalid_path", e.to_string()))?;
        let m = app.store.repo_by_id(&c.org, &id).ok_or_else(|| Problem::not_found("repository_not_found", "repository not found"))?;
        return Ok(json(200, &json!({ "repo_id": m.repo_id, "repo_name": m.repo_name, "url": m.repo_name })));
    }
    if let Some(seg) = path.strip_prefix("/api/repos/").filter(|p| !p.contains('/') && method == Method::DELETE) {
        let name = macrofiche_contract::query::decode_segment(seg).map_err(|e| bad("invalid_path", e.to_string()))?;
        let c = claims(app, &req)?;
        authorize(&c, Scope::RepoWrite, Some(&name)).map_err(|e| e.problem())?;
        return delete_repo(app, &c, &name).await;
    }
    let Some((seg, op)) = path.strip_prefix("/api/repos/").and_then(|p| p.split_once('/')) else {
        return Err(Problem::not_found("no_route", format!("no route {method} {path}")));
    };
    let scope = match (&method, op) {
        (&Method::GET | &Method::HEAD, "branch" | "files/metadata" | "file" | "commits" | "merge/preview") => Scope::GitRead,
        (&Method::POST, "commit-pack" | "branches/create" | "merge" | "restore-commit") => Scope::GitWrite,
        (&Method::PUT | &Method::DELETE, "webhook") => Scope::RepoWrite,
        _ => return Err(Problem::not_found("no_route", format!("no route {method} {path}"))),
    };
    let name = macrofiche_contract::query::decode_segment(seg).map_err(|e| bad("invalid_path", e.to_string()))?;
    let c = claims(app, &req)?;
    authorize(&c, scope, Some(&name)).map_err(|e| e.problem())?;
    let h = app.store.repo(&c.org, &name).await.map_err(store_err)?;
    match (method, op) {
        (Method::GET, "branch") => branch(&h, &query).await,
        (Method::GET, "files/metadata") => files_metadata(&h, &query).await,
        (Method::GET, "file") => file(&h, &query, &req, false).await,
        (Method::HEAD, "file") => file(&h, &query, &req, true).await,
        (Method::GET, "commits") => commits(&h, &query).await,
        (Method::GET, "merge/preview") => preview(&h, &query).await,
        (Method::POST, "commit-pack") => commit_pack(app, &h, req).await,
        (Method::POST, "branches/create") => create_branch(app, &h, req).await,
        (Method::POST, "merge") => merge(app, &h, req).await,
        (Method::POST, "restore-commit") => restore(app, &h, req).await,
        (Method::PUT, "webhook") => put_webhook(&h, req).await,
        (Method::DELETE, "webhook") => delete_webhook(&h, &query).await,
        (m, op) => Err(Problem::not_found("no_route", format!("no route {m} {op}"))),
    }
}

/// A small body, whole, within `max` and its deadline.
async fn small_body(req: Request<Incoming>, max: usize) -> Result<Bytes, Problem> {
    let collect = Limited::new(req.into_body(), max).collect();
    match tokio::time::timeout(body_deadline(max as u64), collect).await {
        Err(_) => Err(bad("body_timeout", "the body did not arrive in time")),
        Ok(Err(e)) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => Err(Problem::new(413, "payload_too_large", format!("a body of at most {max} bytes"))),
        Ok(Err(e)) => Err(bad("invalid_body", format!("the body: {e}"))),
        Ok(Ok(c)) => Ok(c.to_bytes()),
    }
}

async fn json_body(req: Request<Incoming>) -> Result<Value, Problem> {
    let b = small_body(req, JSON_BODY_BYTES_MAX).await?;
    let v: Value = serde_json::from_slice(&b).map_err(|e| bad("invalid_json", format!("the body is not JSON: {e}")))?;
    if !v.is_object() {
        return Err(bad("invalid_json", "the body is a JSON object"));
    }
    Ok(v)
}

fn opt_str<'a>(v: &'a Value, k: &str) -> Result<Option<&'a str>, Problem> {
    match &v[k] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err(bad("invalid_field", format!("{k} is a string"))),
    }
}

fn opt_bool(v: &Value, k: &str) -> Result<bool, Problem> {
    match &v[k] {
        Value::Null => Ok(false),
        Value::Bool(b) => Ok(*b),
        _ => Err(bad("invalid_field", format!("{k} is true or false"))),
    }
}

fn branch_name(s: &str, field: &str) -> Result<String, Problem> {
    if names::valid_branch(s) {
        Ok(s.to_string())
    } else {
        Err(bad("invalid_branch", format!("{field} {s:?} is no branch name")))
    }
}

/// `{name, email}` as a commit's person.
fn ident(v: &Value, k: &str) -> Result<Option<Ident>, Problem> {
    match &v[k] {
        Value::Null => Ok(None),
        p => {
            let (Some(n), Some(e)) = (p["name"].as_str(), p["email"].as_str()) else { return Err(bad("invalid_identity", format!("{k} is {{name, email}}"))) };
            Ident::new(n, e).map(Some).map_err(|e| bad("invalid_identity", format!("{k}: {e}")))
        }
    }
}

fn message(v: &Value) -> Result<String, Problem> {
    let m = opt_str(v, "commit_message")?.unwrap_or("").to_string();
    if m.len() > MESSAGE_BYTES_MAX || m.contains('\0') {
        return Err(bad("invalid_message", format!("commit_message is at most {MESSAGE_BYTES_MAX} bytes, with no NUL")));
    }
    Ok(m)
}

// ---- 5.1 to 5.3: repositories ----

async fn create_repo(app: &App, c: &Claims, q: &Query, req: Request<Incoming>) -> Result<Resp, Problem> {
    authorize(c, Scope::RepoWrite, None).map_err(|e| e.problem())?;
    let body = json_body(req).await?;
    let claim = c.repo.clone().ok_or_else(|| AuthError::RepoClaimMissing.problem())?;
    if let Some(n) = opt_str(&body, "repo_name")?.filter(|n| !n.is_empty()) {
        if n != claim {
            return Err(AuthError::RepoNameMismatch.problem());
        }
    }
    if !body["base_repo"].is_null() || !body["upstream"].is_null() {
        return Err(bad("unsupported", "macrofiche does not copy or sync repositories (base_repo, upstream)"));
    }
    let default_branch = opt_str(&body, "default_branch")?.or(q.get("default_branch")).filter(|b| !b.is_empty()).unwrap_or("main");
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let m = app.store.create_repo(&c.org, &claim, default_branch, now_ms).await.map_err(store_err)?;
    Ok(json(200, &json!({ "repo_id": m.repo_id, "repo_name": m.repo_name, "http_url": m.repo_name, "message": "repository created" })))
}

fn list_repos(app: &App, c: &Claims, q: &Query) -> Result<Resp, Problem> {
    authorize(c, Scope::OrgRead, None).map_err(|e| e.problem())?;
    let limit = q.limit("limit", REPOS_LIMIT_DEFAULT, REPOS_LIMIT_MAX).map_err(|e| bad("invalid_query", e.to_string()))?;
    let after = match q.get("cursor").filter(|c| !c.is_empty()) {
        None => None,
        Some(cur) => match Cursor::decode(cur) {
            Some(Cursor::Repos { created_ms, repo_id }) => Some((created_ms, repo_id)),
            _ => return Err(bad("invalid_cursor", "invalid cursor")),
        },
    };
    let filter = q.get("q").map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
    let (page, more) = app.store.repos_page(&c.org, after.as_ref().map(|(ms, id)| (*ms, id.as_str())), limit, filter.as_deref());
    let repos: Vec<Value> = page
        .iter()
        .map(|m| json!({ "repo_id": m.repo_id, "repo_name": m.repo_name, "url": m.repo_name, "default_branch": m.default_branch, "created_at": rfc3339(m.created_at_ms.div_euclid(1000)) }))
        .collect();
    let mut body = json!({ "repos": repos, "has_more": more });
    if more {
        let last = page.last().expect("more follow a page that has repos");
        body["next_cursor"] = Cursor::Repos { created_ms: last.created_at_ms, repo_id: last.repo_id.clone() }.encode().into();
    }
    Ok(json(200, &body))
}

/// `DELETE /api/repos/{repo}` (contract 5.13): deleted at once, its
/// storage with it. A delete again is 409 `repository_deleted`, as the
/// service answers one.
async fn delete_repo(app: &App, c: &Claims, name: &str) -> Result<Resp, Problem> {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let (store, org, n) = (app.store.clone(), c.org.clone(), name.to_string());
    match detached(async move { store.delete_repo(&org, &n, now_ms).await }).await?.map_err(store_err)? {
        macrofiche_store::Deleted::Now(m) => Ok(json(200, &json!({ "message": "repository deleted", "repo_name": m.repo_name, "repo_id": m.repo_id }))),
        macrofiche_store::Deleted::Already => Err(Problem::conflict("repository_deleted", "repository already deleted")),
    }
}

// ---- reads: 5.4 to 5.7, 5.12, 15.1 ----

async fn branch(h: &RepoHandle, q: &Query) -> Result<Resp, Problem> {
    let name = branch_name(q.get("name").ok_or_else(|| bad("missing_name", "name is required"))?, "name")?;
    let eph = q.flag("ephemeral").map_err(|e| bad("invalid_query", e.to_string()))?;
    let Some(tip) = h.branch(&name, eph).await.map_err(store_err)? else {
        return Err(Problem::not_found("branch_not_found", "branch not found"));
    };
    let refname = macrofiche_store::repo::ref_name(&name, eph);
    let created = match h.created_at(&refname) {
        Some(t) => t,
        None => h.commit_info(tip).await.map_err(store_err)?.committer.time,
    };
    Ok(json(200, &json!({ "branch": { "name": name, "head_sha": tip.to_string(), "created_at": rfc3339(created) } })))
}

/// The commit a read names: `ref` (a sha, a branch, a tag), else the
/// default branch.
async fn commit_of(h: &RepoHandle, q: &Query) -> Result<(Oid, String), Problem> {
    let eph = q.flag("ephemeral").map_err(|e| bad("invalid_query", e.to_string()))?;
    let r = q.get("ref").or(q.get("branch")).filter(|r| !r.is_empty()).unwrap_or(&h.meta.default_branch).to_string();
    let c = h.resolve(&r, eph).await.map_err(store_err)?.ok_or_else(|| Problem::not_found("ref_not_found", "ref not found"))?;
    Ok((c, r))
}

async fn files_metadata(h: &RepoHandle, q: &Query) -> Result<Resp, Problem> {
    let limit = q.limit("limit", FILES_LIMIT_DEFAULT, FILES_LIMIT_MAX).map_err(|e| bad("invalid_query", e.to_string()))?;
    let (commit, asked, offset) = match q.get("cursor").filter(|c| !c.is_empty()) {
        Some(cur) => match Cursor::decode(cur) {
            Some(Cursor::Files { commit, offset }) => {
                let c = Oid::parse(&commit).ok_or_else(|| bad("invalid_cursor", "invalid cursor"))?;
                (c, q.get("ref").unwrap_or(&commit).to_string(), offset)
            }
            _ => return Err(bad("invalid_cursor", "invalid cursor")),
        },
        None => {
            let (c, r) = commit_of(h, q).await?;
            (c, r, 0)
        }
    };
    let under = match q.get("path").filter(|p| !p.is_empty()) {
        Some(p) => {
            names::check_path(p).map_err(|f| bad("invalid_path", f.describe()))?;
            Some(p.to_string())
        }
        None => None,
    };
    let listing = h.listing(commit).await.map_err(store_err)?;
    let entries: Vec<&macrofiche_store::repo::ListEntry> = listing.entries.iter().filter(|e| under.as_ref().is_none_or(|u| e.path == *u || e.path.strip_prefix(u.as_str()).is_some_and(|r| r.starts_with('/')))).collect();
    if offset > entries.len() {
        return Err(bad("invalid_cursor", "invalid cursor"));
    }
    let page = &entries[offset..(offset + limit).min(entries.len())];
    let more = offset + page.len() < entries.len();
    let orders = h.orders(&listing).await.map_err(store_err)?;
    let mut commits = serde_json::Map::new();
    for e in page {
        let k = e.last_commit.to_string();
        if commits.contains_key(&k) {
            continue;
        }
        let ci = h.commit_info(e.last_commit).await.map_err(store_err)?;
        commits.insert(k, json!({ "author": ci.author.name, "date": rfc3339(ci.author.time), "message": ci.message, "order": orders.get(&e.last_commit).copied().unwrap_or(u32::MAX) }));
    }
    let files: Vec<Value> = page
        .iter()
        .map(|e| json!({ "path": e.path, "mode": macrofiche_git::parse::mode::text(e.mode), "type": e.kind, "size": e.size, "last_commit_sha": e.last_commit.to_string() }))
        .collect();
    let mut body = json!({ "files": files, "commits": commits, "ref": asked, "has_more": more });
    if more {
        body["next_cursor"] = Cursor::Files { commit: commit.to_string(), offset: offset + page.len() }.encode().into();
    }
    Ok(json(200, &body))
}

/// A body fed from a channel: a streamed file, a clone's pack.
struct ChannelBody(tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>);

pub fn channel_body(rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Body {
    ChannelBody(rx).boxed()
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|r| r.map(Frame::data)))
    }
}

async fn file(h: &Arc<RepoHandle>, q: &Query, req: &Request<Incoming>, head: bool) -> Result<Resp, Problem> {
    let path = q.get("path").ok_or_else(|| bad("missing_path", "path is required"))?;
    names::check_path(path).map_err(|f| bad("invalid_path", f.describe()))?;
    let (commit, _) = commit_of(h, q).await?;
    let f = h.file(commit, path).await.map_err(store_err)?.ok_or_else(|| Problem::not_found("file_not_found", "file not found"))?;
    let modified = h.commit_info(f.last_commit).await.map_err(store_err)?.committer.time;
    let etag = format!("\"{}\"", f.blob);
    let inm = req.headers().get("if-none-match").and_then(|v| v.to_str().ok());
    let not_modified = inm.is_some_and(|v| v.trim() == "*" || v.split(',').any(|t| t.trim().trim_start_matches("W/") == etag));
    let body = if head || not_modified {
        empty()
    } else {
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_DEPTH);
        let h = h.clone();
        let blob = f.blob;
        tokio::spawn(async move {
            let mut reader = match h.open_blob(blob).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    return;
                }
            };
            // Bounded by the blob: each pass sends a chunk or ends.
            loop {
                let item = match reader.next().await {
                    Ok(Some(chunk)) => Ok(Bytes::from(chunk)),
                    Ok(None) => return,
                    Err(e) => Err(std::io::Error::other(e.to_string())),
                };
                let failed = item.is_err();
                // A client that takes nothing for SEND_IDLE, or is gone,
                // ends the stream; the reader goes with it.
                match tokio::time::timeout(SEND_IDLE, tx.send(item)).await {
                    Ok(Ok(())) if !failed => {}
                    _ => return,
                }
            }
        });
        ChannelBody(rx).boxed()
    };
    let mut r = Response::new(body);
    if not_modified {
        *r.status_mut() = hyper::StatusCode::NOT_MODIFIED;
    } else {
        header(&mut r, "content-type", "application/octet-stream");
        header(&mut r, "content-length", &f.size.to_string());
    }
    header(&mut r, "etag", &etag);
    header(&mut r, "x-blob-sha", &f.blob.to_string());
    header(&mut r, "x-last-commit-sha", &f.last_commit.to_string());
    header(&mut r, "last-modified", &http_date(modified));
    Ok(r)
}

fn commit_json(ci: &macrofiche_store::repo::CommitInfo) -> Value {
    json!({
        "sha": ci.sha.to_string(),
        "message": ci.message,
        "author_name": ci.author.name,
        "author_email": ci.author.email,
        "committer_name": ci.committer.name,
        "committer_email": ci.committer.email,
        "date": rfc3339(ci.author.time),
        "parent_shas": ci.parents.iter().map(Oid::to_string).collect::<Vec<_>>(),
    })
}

async fn commits(h: &RepoHandle, q: &Query) -> Result<Resp, Problem> {
    let limit = q.limit("limit", COMMITS_LIMIT_DEFAULT, COMMITS_LIMIT_MAX).map_err(|e| bad("invalid_query", e.to_string()))?;
    let (tip, offset) = match q.get("cursor").filter(|c| !c.is_empty()) {
        Some(cur) => match Cursor::decode(cur) {
            Some(Cursor::Commits { tip, offset }) if offset <= COMMITS_SKIP_MAX => (Oid::parse(&tip).ok_or_else(|| bad("invalid_cursor", "invalid cursor"))?, offset),
            _ => return Err(bad("invalid_cursor", "invalid cursor")),
        },
        None => (commit_of(h, q).await?.0, 0),
    };
    let (list, more) = h.history(tip, offset, limit).await.map_err(store_err)?;
    let mut body = json!({ "commits": list.iter().map(|c| commit_json(c)).collect::<Vec<_>>(), "has_more": more });
    if more {
        body["next_cursor"] = Cursor::Commits { tip: tip.to_string(), offset: offset + list.len() }.encode().into();
    }
    Ok(json(200, &body))
}

async fn preview(h: &RepoHandle, q: &Query) -> Result<Resp, Problem> {
    let flag = |k| q.flag(k).map_err(|e| bad("invalid_query", e.to_string()));
    let source = branch_name(q.get("source_branch").ok_or_else(|| bad("missing_source_branch", "source_branch is required"))?, "source_branch")?;
    let target = branch_name(q.get("target_branch").ok_or_else(|| bad("missing_target_branch", "target_branch is required"))?, "target_branch")?;
    let p = h.preview(&source, flag("source_is_ephemeral")?, &target, flag("target_is_ephemeral")?).await.map_err(store_err)?;
    let mut body = json!({
        "status": if p.conflicts.is_empty() { "clean" } else { "conflicted" },
        "result": p.kind.name(),
        "source_branch": source,
        "target_branch": target,
        "source_tip_sha": p.source_tip.to_string(),
        "target_tip_sha": p.target_tip.to_string(),
        "merge_base_sha": p.base.map(|b| b.to_string()).unwrap_or_default(),
        "conflict_paths": p.conflicts,
        "conflicts": [],
    });
    if flag("include_content")? {
        body["filtered_conflicts"] = p.conflicts.iter().map(|c| json!({ "path": c, "reason": "macrofiche does not render conflict content" })).collect::<Vec<_>>().into();
    }
    Ok(json(200, &body))
}

// ---- writes: 5.8 to 5.11 ----

/// A commit pack's answer (`CommitWriteResponse`), for success and for
/// the service's refusals in that shape.
#[allow(clippy::too_many_arguments)]
fn pack_answer(status: u16, branch: &str, old: Option<Oid>, new: Option<Oid>, tree: Option<Oid>, pack_bytes: u64, blob_count: usize, result_status: &str, message: Option<&str>) -> Resp {
    let success = (200..300).contains(&status);
    let mut result = json!({ "target_branch": branch, "branch": branch, "old_sha": hex(old), "new_sha": hex(new), "success": success, "status": result_status });
    let mut body = json!({
        "commit": { "commit_sha": if success { hex(new) } else { ZERO_SHA.into() }, "tree_sha": hex(tree), "target_branch": branch, "pack_bytes": pack_bytes, "blob_count": blob_count },
    });
    if let Some(m) = message {
        result["message"] = m.into();
        body["error"] = m.into();
        body["detail"] = m.into();
    }
    body["result"] = result;
    json(status, &body)
}

/// What the service says of a commit that would change nothing.
const NOTHING_TO_COMMIT: &str = "no changes to commit";

/// A write that would change nothing, refused as the service refuses a
/// commit pack that does (contract 5.8): 412, its body exactly the
/// service's (read from a preview by fragment PR #163, 2026-10-05), with
/// no commit and every sha empty. fragment's cell knows it by
/// `result.status` and `result.message`, and answers the branch's tip.
fn nothing_to_commit() -> Resp {
    json(
        412,
        &json!({
            "commit": null,
            "result": { "target_branch": "", "branch": "", "old_sha": "", "new_sha": "", "success": false, "status": "precondition_failed", "message": NOTHING_TO_COMMIT },
        }),
    )
}

fn pack_refusal(e: &PackError, branch: &str) -> Resp {
    match e.shape() {
        Shape::Problem => problem_resp(&Problem::new(e.status(), "invalid_pack", e.to_string())),
        Shape::Result => pack_answer(e.status(), branch, None, None, None, 0, 0, e.result_status(), Some(&e.to_string())),
    }
}

/// A request's spool directory, removed when the request ends.
struct Spool(PathBuf);

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn announce(app: &App, h: &RepoHandle, moved: Option<Moved>) {
    announce_moves(app, h, moved.into_iter().collect());
}

/// Queues the push webhook of each move: one delivery per moved branch to
/// each of the org's hooks and the repository's own (contract section 6).
pub fn announce_moves(app: &App, h: &RepoHandle, moved: Vec<Moved>) {
    if moved.is_empty() {
        return;
    }
    let repo_hooks = read_repo_hooks(h);
    let org_hooks = app.org_hooks.get(&h.meta.org).map(Vec::as_slice).unwrap_or(&[]);
    if org_hooks.is_empty() && repo_hooks.is_empty() {
        return;
    }
    for m in moved {
        let body = webhook::body(&webhook::Push {
            org: &h.meta.org,
            repo_id: &h.meta.repo_id,
            repo_name: &h.meta.repo_name,
            ref_name: &m.ref_name,
            before: hex(m.before),
            after: m.after.to_string(),
            pushed_at: rfc3339(now_s()),
        });
        for d in webhook::deliveries(org_hooks, &repo_hooks, &body) {
            app.hooks.send(d);
        }
    }
}

/// Runs a write to its end on a task of its own: once a request is whole,
/// its client going away does not stop it halfway (a client whose answer
/// is lost re-reads and finds the write landed, contract section 9).
async fn detached<T: Send + 'static>(f: impl std::future::Future<Output = T> + Send + 'static) -> Result<T, Problem> {
    tokio::spawn(f).await.map_err(|e| Problem::internal(format!("a write's task: {e}")))
}

async fn commit_pack(app: &App, h: &Arc<RepoHandle>, req: Request<Incoming>) -> Result<Resp, Problem> {
    let _slot = tokio::time::timeout(PACK_SLOT_WAIT, app.packs.clone().acquire_owned())
        .await
        .map_err(|_| Problem::unavailable("too many commit packs are uploading"))?
        .expect("never closed");
    let declared: Option<u64> = req.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    if declared.is_some_and(|d| d > PACK_BODY_BYTES_MAX) {
        return Ok(pack_refusal(&PackError::TooLarge(format!("a pack body of at most {PACK_BODY_BYTES_MAX} bytes")), ""));
    }
    let deadline = Instant::now() + body_deadline(declared.unwrap_or(PACK_BODY_BYTES_MAX));
    let spool = Spool(app.store.spool_dir().map_err(store_err)?);
    tokio::fs::create_dir_all(&spool.0).await.map_err(|e| store_err(e.into()))?;
    let mut parser = Parser::new();
    let mut meta = None;
    let mut current: Option<(usize, tokio::fs::File)> = None;
    let mut body = req.into_body();
    let mut total = 0u64;
    let branch_of = |m: &Option<macrofiche_pack::Metadata>| m.as_ref().map(|m| m.target_branch.clone()).unwrap_or_default();
    // Bounded by the body: PACK_BODY_BYTES_MAX and its deadline.
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = match tokio::time::timeout(BODY_IDLE.min(left), body.frame()).await {
            Err(_) => return Err(bad("body_timeout", "the pack stalled or passed its deadline")),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(bad("invalid_body", format!("the pack's body: {e}"))),
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else { continue };
        total += data.len() as u64;
        if total > PACK_BODY_BYTES_MAX {
            return Ok(pack_refusal(&PackError::TooLarge(format!("a pack body of at most {PACK_BODY_BYTES_MAX} bytes")), &branch_of(&meta)));
        }
        let events = match parser.push(&data) {
            Ok(e) => e,
            Err(e) => return Ok(pack_refusal(&e, &branch_of(&meta))),
        };
        spool_events(events, &spool.0, &mut meta, &mut current).await?;
    }
    let events = match parser.finish() {
        Ok(e) => e,
        Err(e) => return Ok(pack_refusal(&e, &branch_of(&meta))),
    };
    spool_events(events, &spool.0, &mut meta, &mut current).await?;
    if let Some((_, mut f)) = current.take() {
        f.flush().await.map_err(|e| store_err(e.into()))?;
    }
    let meta = meta.expect("finish refuses a pack without metadata");
    let streams = parser.streams();
    let files: Vec<PackFile> = meta
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| PackFile {
            path: f.path.clone(),
            change: match &f.op {
                Op::Upsert { mode, .. } => PackChange::Upsert { mode: *mode, spool: spool.0.join(i.to_string()), len: streams[i].bytes },
                Op::Delete => PackChange::Delete,
            },
        })
        .collect();
    let person = |p: &Option<macrofiche_pack::Person>, k: &str| -> Result<Option<Ident>, Problem> {
        p.as_ref().map(|p| Ident::new(&p.name, &p.email).map_err(|e| bad("invalid_identity", format!("{k}: {e}")))).transpose()
    };
    let author = match person(&meta.author, "author") {
        Ok(a) => a,
        Err(p) => return Ok(pack_answer(400, &meta.target_branch, None, None, None, 0, 0, "invalid", Some(&p.detail))),
    };
    let committer = match person(&meta.committer, "committer") {
        Ok(a) => a,
        Err(p) => return Ok(pack_answer(400, &meta.target_branch, None, None, None, 0, 0, "invalid", Some(&p.detail))),
    };
    let task_h = h.clone();
    let (written, meta) = detached(async move {
        let r = task_h.commit_pack(&meta, &files, author, committer, now_s()).await;
        drop(spool);
        (r, meta)
    })
    .await?;
    let (outcome, moved) = written.map_err(store_err)?;
    let resp = match outcome {
        CommitOutcome::Committed(c) => pack_answer(201, &c.branch, c.old, Some(c.new), Some(c.tree), total, c.blob_count, "ok", None),
        CommitOutcome::Moved { current } => pack_answer(409, &meta.target_branch, current, current, None, 0, 0, "precondition_failed", Some("expected branch head did not match current tip")),
        CommitOutcome::Unchanged { .. } => nothing_to_commit(),
    };
    announce(app, h, moved);
    Ok(resp)
}

/// Writes each chunk to its file's spool, keeping the file being written
/// open while its chunks come in a row.
async fn spool_events(events: Vec<Event>, dir: &std::path::Path, meta: &mut Option<macrofiche_pack::Metadata>, current: &mut Option<(usize, tokio::fs::File)>) -> Result<(), Problem> {
    for e in events {
        match e {
            Event::Metadata(m) => *meta = Some(m),
            Event::Chunk { file, data } => {
                if current.as_ref().is_none_or(|(i, _)| *i != file) {
                    if let Some((_, mut f)) = current.take() {
                        f.flush().await.map_err(|e| store_err(e.into()))?;
                    }
                    let f = tokio::fs::OpenOptions::new().create(true).append(true).open(dir.join(file.to_string())).await.map_err(|e| store_err(e.into()))?;
                    *current = Some((file, f));
                }
                let (_, f) = current.as_mut().expect("just opened");
                f.write_all(&data).await.map_err(|e| store_err(e.into()))?;
            }
        }
    }
    Ok(())
}

async fn create_branch(app: &App, h: &Arc<RepoHandle>, req: Request<Incoming>) -> Result<Resp, Problem> {
    let b = json_body(req).await?;
    let target = branch_name(opt_str(&b, "target_branch")?.ok_or_else(|| bad("missing_target_branch", "target_branch is required"))?, "target_branch")?;
    let base = opt_str(&b, "base_ref")?.or(opt_str(&b, "base_branch")?).ok_or_else(|| bad("missing_base_ref", "base_ref is required"))?.to_string();
    if opt_bool(&b, "force")? {
        return Err(bad("unsupported", "macrofiche does not force a branch over an existing one"));
    }
    let eph = opt_bool(&b, "target_is_ephemeral")?;
    let (task_h, t, base_eph) = (h.clone(), target.clone(), opt_bool(&b, "base_is_ephemeral")?);
    let (outcome, moved) = detached(async move { task_h.create_branch(&t, eph, &base, base_eph, now_s()).await }).await?.map_err(store_err)?;
    match outcome {
        CreateOutcome::Exists => Err(Problem::conflict("branch_exists", format!("branch already exists: {target}"))),
        CreateOutcome::Created(at) => {
            announce(app, h, moved);
            Ok(json(200, &json!({ "message": "branch created", "target_branch": target, "target_is_ephemeral": eph, "commit_sha": at.to_string() })))
        }
    }
}

/// The merge's 409 (`MergeConflict`), with problem fields for clients that
/// read `detail`.
fn merge_conflict(conflict_type: &str, error: &str, paths: &[String], base: Option<Oid>) -> Resp {
    let mut v = json!({
        "error": error, "conflict_type": conflict_type, "conflict_paths": paths,
        "type": "about:blank", "title": "Conflict", "status": 409, "code": conflict_type, "detail": error,
    });
    if let Some(b) = base {
        v["merge_base_sha"] = b.to_string().into();
    }
    json(409, &v)
}

async fn merge(app: &App, h: &Arc<RepoHandle>, req: Request<Incoming>) -> Result<Resp, Problem> {
    let b = json_body(req).await?;
    let target = branch_name(opt_str(&b, "target_branch")?.ok_or_else(|| bad("missing_target_branch", "target_branch is required"))?, "target_branch")?;
    let source = opt_str(&b, "source_ref")?.or(opt_str(&b, "source_branch")?).ok_or_else(|| bad("missing_source_ref", "source_ref is required"))?.to_string();
    let strategy = opt_str(&b, "strategy")?.and_then(Strategy::parse).ok_or_else(|| bad("invalid_strategy", "strategy is merge, ff_only or ff_prefer"))?;
    let expected = match opt_str(&b, "expected_target_sha")? {
        None => None,
        Some(s) => Some(Oid::parse(s).ok_or_else(|| bad("invalid_sha", "expected_target_sha is a 40-hex sha"))?),
    };
    let squash = opt_bool(&b, "squash")?;
    if squash && strategy == Strategy::FfOnly {
        return Err(bad("invalid_strategy", "squash is incompatible with ff_only"));
    }
    let r = MergeRequest {
        target: target.clone(),
        target_ephemeral: opt_bool(&b, "target_is_ephemeral")?,
        source: source.clone(),
        source_ephemeral: opt_bool(&b, "source_is_ephemeral")?,
        strategy,
        expected,
        message: message(&b)?,
        author: ident(&b, "author")?,
        committer: ident(&b, "committer")?,
        squash,
        allow_unrelated: opt_bool(&b, "allow_unrelated_histories")?,
    };
    let (target_eph, source_eph) = (r.target_ephemeral, r.source_ephemeral);
    let task_h = h.clone();
    let (outcome, moved) = detached(async move { task_h.merge(r, now_s()).await }).await?.map_err(store_err)?;
    let resp = match outcome {
        MergeOutcome::Done(d) => json(
            200,
            &json!({
                "commit_sha": d.new.to_string(),
                "tree_sha": d.tree.to_string(),
                "result": d.kind.name(),
                "promoted_commits": d.promoted,
                "merge_base_sha": d.base.map(|b| b.to_string()).unwrap_or_default(),
                "source": { "branch": source, "ref": source, "ephemeral": source_eph, "sha": d.source.to_string() },
                "target": { "branch": target, "ephemeral": target_eph, "old_sha": d.old.to_string(), "new_sha": d.new.to_string() },
            }),
        ),
        MergeOutcome::TargetMoved { current, base } => merge_conflict("target_moved", &format!("expected target sha did not match the current tip {current}"), &[], base),
        MergeOutcome::Conflict { base, paths } => merge_conflict("merge_conflict", "merge conflict", &paths, base),
        MergeOutcome::NotFastForward { base } => merge_conflict("not_fast_forward", "not a fast-forward, and the strategy is ff_only", &[], base),
        MergeOutcome::Unchanged { .. } => nothing_to_commit(),
    };
    announce(app, h, moved);
    Ok(resp)
}

async fn restore(app: &App, h: &Arc<RepoHandle>, req: Request<Incoming>) -> Result<Resp, Problem> {
    let b = small_body(req, JSON_BODY_BYTES_MAX).await?;
    let line = b.split(|c| *c == b'\n').find(|l| !l.trim_ascii().is_empty()).ok_or_else(|| bad("invalid_body", "a restore is one metadata line"))?;
    let v: Value = serde_json::from_slice(line).map_err(|_| bad("invalid_body", "a restore's line is not JSON"))?;
    let m = if v["metadata"].is_object() { v["metadata"].clone() } else { v };
    let target = branch_name(opt_str(&m, "target_branch")?.ok_or_else(|| bad("missing_target_branch", "target_branch is required"))?, "target_branch")?;
    let base = opt_str(&m, "base_ref")?.ok_or_else(|| bad("missing_base_ref", "base_ref is required"))?.to_string();
    let expected = names::parse_expected(opt_str(&m, "expected_target_sha")?.or(opt_str(&m, "expected_head_sha")?)).ok_or_else(|| bad("invalid_sha", "expected_target_sha is a 40-hex sha"))?;
    let eph = opt_bool(&m, "target_is_ephemeral")? || opt_bool(&m, "ephemeral")?;
    let (task_h, t, msg, author, committer) = (h.clone(), target.clone(), message(&m)?, ident(&m, "author")?, ident(&m, "committer")?);
    let (outcome, moved) = detached(async move { task_h.restore(&t, eph, &base, &expected, &msg, author, committer, now_s()).await }).await?.map_err(store_err)?;
    let resp = match outcome {
        RestoreOutcome::Done(c) => pack_answer(201, &target, c.old, Some(c.new), Some(c.tree), 0, 0, "ok", None),
        RestoreOutcome::Moved { current } => pack_answer(409, &target, Some(current), Some(current), None, 0, 0, "precondition_failed", Some("expected branch head did not match current tip")),
        RestoreOutcome::Refused { tip, why } => pack_answer(412, &target, Some(tip), Some(tip), None, 0, 0, "precondition_failed", Some(why)),
    };
    announce(app, h, moved);
    Ok(resp)
}

// ---- macrofiche's extension: a repository's own webhooks ----

const HOOKS_FILE: &str = "macrofiche/webhooks.json";

fn read_repo_hooks(h: &RepoHandle) -> Vec<OrgHook> {
    let Ok(bytes) = std::fs::read(h.dir.join(HOOKS_FILE)) else { return Vec::new() };
    let v: Vec<BTreeMap<String, String>> = serde_json::from_slice(&bytes).unwrap_or_default();
    v.into_iter().filter_map(|m| Some(OrgHook { url: m.get("url")?.clone(), secret: m.get("secret")?.clone().into_bytes() })).collect()
}

fn write_repo_hooks(h: &RepoHandle, hooks: &[OrgHook]) -> Result<(), Problem> {
    let v: Vec<Value> = hooks.iter().map(|k| json!({ "url": k.url, "secret": String::from_utf8_lossy(&k.secret) })).collect();
    let path = h.dir.join(HOOKS_FILE);
    macrofiche_store::repo::write_durably(&path, serde_json::to_string(&v).expect("hooks serialize").as_bytes()).map_err(|e| store_err(e.into()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| store_err(e.into()))
}

/// `PUT /api/repos/{repo}/webhook {url, secret}`: macrofiche's own route,
/// not the service's (it registers hooks in its dashboard): pushes to
/// this repository are delivered to `url`, signed with `secret`.
async fn put_webhook(h: &RepoHandle, req: Request<Incoming>) -> Result<Resp, Problem> {
    let b = json_body(req).await?;
    let url = opt_str(&b, "url")?.filter(|u| crate::config::valid_hook_url(u)).ok_or_else(|| bad("invalid_url", "url is an http(s) URL"))?.to_string();
    let secret = opt_str(&b, "secret")?.filter(|s| !s.is_empty() && s.len() <= crate::config::SECRET_BYTES_MAX).ok_or_else(|| bad("invalid_secret", "secret is 1 to 4096 bytes"))?;
    let _w = h.write().await.map_err(store_err)?;
    let mut hooks = read_repo_hooks(h);
    hooks.retain(|k| k.url != url);
    if hooks.len() >= webhook::REPO_HOOKS_MAX {
        return Err(Problem::conflict("too_many_webhooks", format!("a repository has at most {} webhooks", webhook::REPO_HOOKS_MAX)));
    }
    hooks.push(OrgHook { url: url.clone(), secret: secret.as_bytes().to_vec() });
    write_repo_hooks(h, &hooks)?;
    Ok(json(200, &json!({ "url": url, "registered": true })))
}

async fn delete_webhook(h: &RepoHandle, q: &Query) -> Result<Resp, Problem> {
    let url = q.get("url").ok_or_else(|| bad("missing_url", "url is required"))?.to_string();
    let _w = h.write().await.map_err(store_err)?;
    let mut hooks = read_repo_hooks(h);
    let before = hooks.len();
    hooks.retain(|k| k.url != url);
    if hooks.len() == before {
        return Err(Problem::not_found("webhook_not_found", "no such webhook"));
    }
    write_repo_hooks(h, &hooks)?;
    Ok(json(200, &json!({ "url": url, "registered": false })))
}
