//! git's smart HTTP (docs/design.md, phase 5): `git clone`, `fetch` and
//! `push` at `<base>/<repo>.git`, with the same JWT as the REST API (as a
//! Basic password, as the service's `https://t:<JWT>@…/<repo>.git`, or as
//! a bearer). It is git's own protocol, served as git's `http-backend`
//! serves it: `upload-pack` and `receive-pack` with `--stateless-rpc`,
//! jailed, protocol v2 passed through `Git-Protocol`.
//!
//! A push keeps the REST API's rules (docs/library.md, Risks: two write
//! paths): it runs under the repository's write lock, so it and a commit
//! pack never interleave; receive-pack's own ref transaction is the
//! compare-and-swap of each ref the push names; the refs it moved get
//! their last-commit records and their webhooks, as any write's do;
//! macrofiche's own refs and the ephemeral namespace are hidden, so a push
//! cannot touch them (`transfer.hideRefs`).

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use http_body_util::BodyExt;
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response};
use macrofiche_auth::{authorize, verify, AuthError, Claims, Scope};
use macrofiche_contract::time::now_s;
use macrofiche_contract::Problem;
use macrofiche_git::jail::Limits;
use macrofiche_git::linux::process::Pipes;
use macrofiche_git::Oid;
use macrofiche_store::repo::Moved;
use macrofiche_store::RepoHandle;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::api::{announce_moves, body_deadline, problem_resp, App, Resp, BODY_IDLE};

/// A request that is gzipped (git's client gzips its small ones) is read
/// whole, at most this compressed and this decompressed.
pub const GZIP_BYTES_MAX: usize = 64 << 20;
pub const GUNZIP_BYTES_MAX: u64 = 256 << 20;
/// A push's pack, at most (receive-pack refuses past it too).
pub const PUSH_BYTES_MAX: u64 = 2 << 30;
/// receive-pack's answer (its report and progress), kept whole.
pub const REPORT_BYTES_MAX: usize = 16 << 20;
/// How long a clone's or a push's git may run.
pub const TRANSFER_DEADLINE: Duration = Duration::from_secs(3600);
/// The limits a transfer's git runs under: pack-objects's delta search.
pub const TRANSFER_LIMITS: Limits = Limits { cpu_s: 1800, data_bytes: 2 << 30, fsize_bytes: 4 << 30, nofile: 4096 };

/// Which of git's services.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    fn parse(s: &str) -> Option<Service> {
        match s {
            "git-upload-pack" => Some(Service::UploadPack),
            "git-receive-pack" => Some(Service::ReceivePack),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Service::UploadPack => "upload-pack",
            Service::ReceivePack => "receive-pack",
        }
    }
    fn scope(self) -> Scope {
        match self {
            Service::UploadPack => Scope::GitRead,
            Service::ReceivePack => Scope::GitWrite,
        }
    }
}

/// A smart-HTTP path: `/<repo>.git/info/refs`, `/<repo>.git/git-upload-pack`,
/// `/<repo>.git/git-receive-pack`, the repository's name (which may hold
/// `/`) percent-decoded once.
pub fn parse_path(path: &str) -> Option<(String, &str)> {
    let (repo, op) = path.strip_prefix('/')?.rsplit_once(".git/")?;
    if !matches!(op, "info/refs" | "git-upload-pack" | "git-receive-pack") || repo.is_empty() {
        return None;
    }
    let name = macrofiche_contract::query::decode_segment(repo).ok()?;
    macrofiche_contract::names::valid_repo_name(&name).then_some((name, op))
}

/// The token from Basic (its password) or Bearer credentials.
pub fn credentials(header: Option<&str>) -> Option<String> {
    let h = header?.trim();
    let (scheme, rest) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.trim().to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (_, password) = text.split_once(':')?;
        return Some(password.to_string());
    }
    None
}

/// 401 with a Basic challenge, so git sends the credentials its URL holds.
fn challenge(detail: &str) -> Resp {
    let mut r = problem_resp(&Problem::new(401, "unauthorized", detail));
    r.headers_mut().insert("www-authenticate", "Basic realm=\"macrofiche\"".parse().expect("a header"));
    r
}

fn claims(app: &App, req: &Request<Incoming>) -> Result<Claims, Box<Resp>> {
    let Some(token) = credentials(req.headers().get("authorization").and_then(|v| v.to_str().ok())) else { return Err(Box::new(challenge("credentials are needed: the JWT as the password"))) };
    verify(&app.keyring, &token, now_s()).map_err(|e| {
        Box::new(match e {
            AuthError::NoBearer => challenge("credentials are needed"),
            e => problem_resp(&e.problem()),
        })
    })
}

fn streamed(content_type: &str, rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Resp {
    let mut r = Response::new(crate::api::channel_body(rx));
    r.headers_mut().insert("content-type", content_type.parse().expect("a header"));
    r.headers_mut().insert("cache-control", "no-cache, max-age=0, must-revalidate".parse().expect("a header"));
    r
}

fn full(content_type: &str, bytes: Vec<u8>) -> Resp {
    let mut r = Response::new(http_body_util::Full::new(Bytes::from(bytes)).map_err(|n: std::convert::Infallible| match n {}).boxed());
    r.headers_mut().insert("content-type", content_type.parse().expect("a header"));
    r.headers_mut().insert("cache-control", "no-cache, max-age=0, must-revalidate".parse().expect("a header"));
    r
}

/// `GIT_PROTOCOL` from the `Git-Protocol` header, if it is git's form.
fn protocol(req: &Request<Incoming>) -> Option<String> {
    let v = req.headers().get("git-protocol")?.to_str().ok()?;
    (v.len() <= 256 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b":=.-_".contains(&b))).then(|| v.to_string())
}

/// Serves a smart-HTTP request.
pub async fn serve(app: &App, req: Request<Incoming>, name: String, op: &str) -> Result<Resp, Problem> {
    let service = match op {
        "info/refs" => {
            let q = macrofiche_contract::Query::parse(req.uri().query()).map_err(|e| Problem::bad_request("invalid_query", e.to_string()))?;
            q.get("service").and_then(Service::parse).ok_or_else(|| Problem::new(403, "dumb_http", "only smart HTTP is served: ?service=git-upload-pack or git-receive-pack"))?
        }
        other => Service::parse(other).expect("parse_path checked it"),
    };
    if (op == "info/refs") != (req.method() == Method::GET) {
        return Err(Problem::new(405, "method_not_allowed", format!("{} {op}", req.method())));
    }
    let c = match claims(app, &req) {
        Ok(c) => c,
        Err(r) => return Ok(*r),
    };
    authorize(&c, service.scope(), Some(&name)).map_err(|e| e.problem())?;
    let h = app.store.repo(&c.org, &name).await.map_err(|e| e.problem())?;
    let env: Vec<(&'static str, String)> = protocol(&req).map(|p| vec![("GIT_PROTOCOL", p)]).unwrap_or_default();
    if op == "info/refs" {
        return advertise(&h, service, env).await;
    }
    let want_type = format!("application/x-git-{}-request", service.name());
    if req.headers().get("content-type").and_then(|v| v.to_str().ok()) != Some(want_type.as_str()) {
        return Err(Problem::new(415, "unsupported_media_type", format!("expected {want_type}")));
    }
    match service {
        Service::UploadPack => upload_pack(&h, req, env).await,
        Service::ReceivePack => receive_pack(app, &h, req, env).await,
    }
}

async fn advertise(h: &RepoHandle, service: Service, env: Vec<(&'static str, String)>) -> Result<Resp, Problem> {
    let v2 = env.iter().any(|(_, v)| v.contains("version=2"));
    let op = if service == Service::UploadPack { "upload-pack" } else { "receive-pack" };
    let out = h
        .git
        .run(op, &[service.name(), "--stateless-rpc", "--http-backend-info-refs", "."], macrofiche_git::RunOpts { env, stdout_max: 64 << 20, ..macrofiche_git::RunOpts::read() })
        .await
        .map_err(|e| Problem::unavailable(e.to_string()))?;
    let stdout = out.ok(op).map_err(|e| Problem::unavailable(e.to_string()))?;
    let mut body = Vec::with_capacity(stdout.len() + 64);
    if !v2 || service == Service::ReceivePack {
        let line = format!("# service=git-{}\n", service.name());
        body.extend_from_slice(format!("{:04x}{line}0000", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(&stdout);
    Ok(full(&format!("application/x-git-{}-advertisement", service.name()), body))
}

/// A request's body, gunzipped when it says so, or spooled.
enum Input {
    Whole(Vec<u8>),
    Stream(Incoming),
    File(std::path::PathBuf),
}

async fn input(req: Request<Incoming>) -> Result<Input, Problem> {
    let gzip = req.headers().get("content-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v == "gzip" || v == "x-gzip");
    if !gzip {
        return Ok(Input::Stream(req.into_body()));
    }
    let body = http_body_util::Limited::new(req.into_body(), GZIP_BYTES_MAX).collect();
    let bytes = tokio::time::timeout(body_deadline(GZIP_BYTES_MAX as u64), body)
        .await
        .map_err(|_| Problem::bad_request("body_timeout", "the body did not arrive in time"))?
        .map_err(|_| Problem::new(413, "payload_too_large", format!("a gzipped body of at most {GZIP_BYTES_MAX} bytes")))?
        .to_bytes();
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..]).take(GUNZIP_BYTES_MAX + 1).read_to_end(&mut out).map_err(|e| Problem::bad_request("invalid_gzip", e.to_string()))?;
    if out.len() as u64 > GUNZIP_BYTES_MAX {
        return Err(Problem::new(413, "payload_too_large", "a body past its decompressed bound"));
    }
    Ok(Input::Whole(out))
}

/// Copies a body into `out`, bounded by `max` bytes, its deadline, and its
/// idle time.
async fn spool_body(mut body: Incoming, out: &mut tokio::fs::File, max: u64) -> Result<(), Problem> {
    let deadline = Instant::now() + body_deadline(max);
    let mut total = 0u64;
    // Bounded by `max` and the deadline.
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = match tokio::time::timeout(BODY_IDLE.min(left), body.frame()).await {
            Err(_) => return Err(Problem::bad_request("body_timeout", "the push stalled or passed its deadline")),
            Ok(None) => return Ok(()),
            Ok(Some(f)) => f.map_err(|e| Problem::bad_request("invalid_body", e.to_string()))?,
        };
        if let Ok(data) = frame.into_data() {
            total += data.len() as u64;
            if total > max {
                return Err(Problem::new(413, "payload_too_large", format!("a push of at most {max} bytes")));
            }
            out.write_all(&data).await.map_err(|e| Problem::unavailable(e.to_string()))?;
        }
    }
}

/// Feeds `input` into `stdin`, bounded by `max` bytes and the body's idle
/// time.
async fn feed(input: Input, mut stdin: tokio::process::ChildStdin, max: u64) -> Result<u64, String> {
    let mut total = 0u64;
    match input {
        Input::Whole(b) => {
            stdin.write_all(&b).await.map_err(|e| e.to_string())?;
            total = b.len() as u64;
        }
        Input::File(path) => {
            let mut f = tokio::fs::File::open(&path).await.map_err(|e| e.to_string())?;
            total = tokio::io::copy(&mut f, &mut stdin).await.map_err(|e| e.to_string())?;
        }
        Input::Stream(mut body) => {
            let deadline = Instant::now() + body_deadline(max);
            // Bounded by `max` and the deadline.
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let frame = match tokio::time::timeout(BODY_IDLE.min(left), body.frame()).await {
                    Err(_) => return Err("the body stalled".into()),
                    Ok(None) => break,
                    Ok(Some(f)) => f.map_err(|e| e.to_string())?,
                };
                if let Ok(data) = frame.into_data() {
                    total += data.len() as u64;
                    if total > max {
                        return Err(format!("a body past {max} bytes"));
                    }
                    stdin.write_all(&data).await.map_err(|e| e.to_string())?;
                }
            }
        }
    }
    stdin.shutdown().await.map_err(|e| e.to_string())?;
    Ok(total)
}

async fn upload_pack(h: &Arc<RepoHandle>, req: Request<Incoming>, env: Vec<(&'static str, String)>) -> Result<Resp, Problem> {
    let input = input(req).await?;
    let mut proc = h.git.spawn("upload-pack", &["upload-pack", "--stateless-rpc", "."], &env, TRANSFER_LIMITS, Pipes { stdin: true, stdout: true }).await.map_err(|e| Problem::unavailable(e.to_string()))?;
    let stdin = proc.stdin().expect("piped");
    let mut stdout = proc.stdout().expect("piped");
    let mut stderr = proc.stderr().expect("piped");
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let work = async {
            let writer = feed(input, stdin, GUNZIP_BYTES_MAX);
            let errs = async {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            };
            let reader = async {
                let mut buf = vec![0u8; 64 * 1024];
                // Bounded by git's output: it ends.
                loop {
                    let n = match stdout.read(&mut buf).await {
                        Ok(0) => return true,
                        Ok(n) => n,
                        Err(_) => return false,
                    };
                    match tokio::time::timeout(crate::api::SEND_IDLE, tx.send(Ok(Bytes::copy_from_slice(&buf[..n])))).await {
                        Ok(Ok(())) => {}
                        _ => return false,
                    }
                }
            };
            let (_, (), done) = tokio::join!(writer, errs, reader);
            done
        };
        // A transfer past its deadline, or a client gone, ends git's group.
        if let Ok(true) = tokio::time::timeout(TRANSFER_DEADLINE, work).await {
            let _ = proc.wait().await;
        }
    });
    Ok(streamed("application/x-git-upload-pack-result", rx))
}

/// A spool removed when its request ends.
struct Spool(std::path::PathBuf);

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn receive_pack(app: &App, h: &Arc<RepoHandle>, req: Request<Incoming>, env: Vec<(&'static str, String)>) -> Result<Resp, Problem> {
    // The push's body is spooled first, as a commit pack's blobs are, so a
    // slow client never holds the repository's write lock.
    let spool = Spool(app.store.spool_dir().map_err(|e| e.problem())?);
    tokio::fs::create_dir_all(&spool.0).await.map_err(|e| Problem::unavailable(e.to_string()))?;
    let file = spool.0.join("push");
    {
        let mut out = tokio::fs::File::create(&file).await.map_err(|e| Problem::unavailable(e.to_string()))?;
        match input(req).await? {
            Input::Whole(b) => out.write_all(&b).await.map_err(|e| Problem::unavailable(e.to_string()))?,
            Input::Stream(body) => spool_body(body, &mut out, PUSH_BYTES_MAX).await?,
            Input::File(_) => unreachable!("input answers no file"),
        }
        out.flush().await.map_err(|e| Problem::unavailable(e.to_string()))?;
    }
    let input = Input::File(file);
    let h2 = h.clone();
    // The push runs on a task of its own to its end, as every write does,
    // under the repository's write lock.
    let (report, moved) = tokio::spawn(async move {
        let _w = h2.write().await.map_err(|e| e.problem())?;
        let before = h2.git.refs("refs/heads/").await.map_err(|e| Problem::unavailable(e.to_string()))?;
        let max_input = format!("receive.maxInputSize={PUSH_BYTES_MAX}");
        let mut proc = h2
            .git
            .spawn("receive-pack", &["-c", &max_input, "receive-pack", "--stateless-rpc", "."], &env, TRANSFER_LIMITS, Pipes { stdin: true, stdout: true })
            .await
            .map_err(|e| Problem::unavailable(e.to_string()))?;
        let stdin = proc.stdin().expect("piped");
        let mut stdout = proc.stdout().expect("piped");
        let mut stderr = proc.stderr().expect("piped");
        let work = async {
            let writer = feed(input, stdin, PUSH_BYTES_MAX);
            let reader = async {
                let mut out = Vec::new();
                let r = (&mut stdout).take(REPORT_BYTES_MAX as u64).read_to_end(&mut out).await;
                r.map(|_| out)
            };
            let errs = async {
                let mut e = Vec::new();
                let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut e).await;
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
                e
            };
            tokio::join!(writer, reader, errs)
        };
        let (fed, report, errs) = tokio::time::timeout(TRANSFER_DEADLINE, work).await.map_err(|_| Problem::unavailable("the push passed its deadline"))?;
        let _ = proc.wait().await;
        if let Err(e) = fed {
            eprintln!("{}", json!({ "event": "push.input", "error": e, "stderr": String::from_utf8_lossy(&errs) }));
        }
        let report = report.map_err(|e| Problem::unavailable(e.to_string()))?;
        let after = h2.git.refs("refs/heads/").await.map_err(|e| Problem::unavailable(e.to_string()))?;
        let moved = moves(&before, &after);
        for m in &moved {
            if !m.after.is_zero() {
                // The pushed commits get their records now, so a listing
                // after the push costs no rebuild.
                h2.ensure_record(m.after).await.map_err(|e| e.problem())?;
            }
        }
        if !moved.is_empty() {
            h2.touched();
        }
        drop(spool);
        Ok::<_, Problem>((report, moved))
    })
    .await
    .map_err(|e| Problem::internal(format!("a push's task: {e}")))??;
    announce_moves(app, h, moved);
    Ok(full("application/x-git-receive-pack-result", report))
}

/// The refs a push moved, created, or deleted.
pub fn moves(before: &[(String, Oid)], after: &[(String, Oid)]) -> Vec<Moved> {
    let old = |r: &str| before.iter().find(|(n, _)| n == r).map(|(_, o)| *o);
    let mut out = Vec::new();
    for (r, o) in after {
        if old(r) != Some(*o) {
            out.push(Moved { ref_name: r.clone(), before: old(r), after: *o });
        }
    }
    for (r, o) in before {
        if !after.iter().any(|(n, _)| n == r) {
            out.push(Moved { ref_name: r.clone(), before: Some(*o), after: macrofiche_git::oid::ZERO });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: smart-HTTP paths name their repository (slashes and escapes
    // included) and their service; anything else is not one. Method:
    // the paths git's client asks for.
    #[test]
    fn paths_and_credentials() {
        assert_eq!(parse_path("/todo--paul.git/info/refs"), Some(("todo--paul".into(), "info/refs")));
        assert_eq!(parse_path("/acme/web.git/git-upload-pack"), Some(("acme/web".into(), "git-upload-pack")));
        assert_eq!(parse_path("/acme%2Fweb.git/git-receive-pack"), Some(("acme/web".into(), "git-receive-pack")));
        for bad in ["/api/repos", "/r.git/objects/info/packs", "/.git/info/refs", "/a b.git/info/refs", "/../x.git/info/refs"] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("t:eyJ.a.b"));
        assert_eq!(credentials(Some(&basic)).as_deref(), Some("eyJ.a.b"));
        assert_eq!(credentials(Some("Bearer eyJ.a.b")).as_deref(), Some("eyJ.a.b"));
        assert_eq!(credentials(Some("Digest x")), None);
        assert_eq!(credentials(None), None);
    }

    // Goal: a push's moves are each ref it created, moved, or deleted,
    // and none it left. Method: before and after.
    #[test]
    fn a_pushs_moves() {
        let o = |n: u8| Oid::from_bytes([n; 20]);
        let before = vec![("refs/heads/main".to_string(), o(1)), ("refs/heads/old".to_string(), o(2)), ("refs/heads/same".to_string(), o(3))];
        let after = vec![("refs/heads/main".to_string(), o(4)), ("refs/heads/new".to_string(), o(5)), ("refs/heads/same".to_string(), o(3))];
        let m = moves(&before, &after);
        assert_eq!(m.len(), 3);
        assert!(m.contains(&Moved { ref_name: "refs/heads/main".into(), before: Some(o(1)), after: o(4) }));
        assert!(m.contains(&Moved { ref_name: "refs/heads/new".into(), before: None, after: o(5) }));
        assert!(m.contains(&Moved { ref_name: "refs/heads/old".into(), before: Some(o(2)), after: macrofiche_git::oid::ZERO }));
    }
}
