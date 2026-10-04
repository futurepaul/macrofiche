//! The push webhook (docs/contract.md, section 6): after every branch
//! move, one delivery to each URL registered for the repository (its
//! org's, from the config, and its own, through macrofiche's extension
//! route), signed `X-Pierre-Signature: t=<unix s>,sha256=<hex>` over
//! `<t>.<body>`, signed again on each attempt, retried with backoff.
//! Delivery is at least once while the process lives, and best effort
//! across a restart: fragment's poll and `refresh` catch a lost one.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Bytes;
use serde_json::json;
use tokio::sync::{mpsc, Semaphore};

use crate::config::OrgHook;

/// Attempts per delivery, and the waits between them.
pub const ATTEMPTS: u32 = 5;
pub const BACKOFF: [Duration; 4] = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8)];
/// One attempt's deadline, connect to answer.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Deliveries waiting, and delivering at once.
pub const QUEUE_MAX: usize = 10_000;
pub const INFLIGHT_MAX: usize = 64;
/// A registered webhook's secret, and hooks per repository (the extension).
pub const REPO_HOOKS_MAX: usize = 4;

/// `X-Pierre-Signature` for `body` at `t`.
pub fn signature(body: &[u8], secret: &[u8], t: i64) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let mut ctx = ring::hmac::Context::with_key(&key);
    ctx.update(format!("{t}.").as_bytes());
    ctx.update(body);
    let tag = ctx.sign();
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("t={t},sha256={hex}")
}

/// What a push says: the union of the fake's body and the service's
/// (contract section 6).
pub struct Push<'a> {
    pub org: &'a str,
    pub repo_id: &'a str,
    pub repo_name: &'a str,
    pub ref_name: &'a str,
    pub before: String,
    pub after: String,
    pub pushed_at: String,
}

pub fn body(p: &Push<'_>) -> Vec<u8> {
    json!({
        "type": "push",
        "org": p.org,
        "repository": { "repo_id": p.repo_id, "repo_name": p.repo_name, "url": p.repo_name, "id": p.repo_id },
        "ref": p.ref_name,
        "before": p.before,
        "after": p.after,
        "pushed_at": p.pushed_at,
    })
    .to_string()
    .into_bytes()
}

/// One delivery: where, signed with what, saying what.
#[derive(Clone)]
pub struct Delivery {
    pub url: String,
    pub secret: Vec<u8>,
    pub body: Vec<u8>,
}

/// The delivery queue and its worker.
#[derive(Clone)]
pub struct Hooks {
    tx: mpsc::Sender<Delivery>,
}

impl Hooks {
    pub fn start() -> Hooks {
        let (tx, mut rx) = mpsc::channel::<Delivery>(QUEUE_MAX);
        let slots = Arc::new(Semaphore::new(INFLIGHT_MAX));
        tokio::spawn(async move {
            // Unbounded by design: the worker serves the process's life.
            while let Some(d) = rx.recv().await {
                let permit = slots.clone().acquire_owned().await.expect("never closed");
                tokio::spawn(async move {
                    deliver(&d).await;
                    drop(permit);
                });
            }
        });
        Hooks { tx }
    }

    /// Queues `d`; a full queue drops it (best effort), said on stderr.
    pub fn send(&self, d: Delivery) {
        if self.tx.try_send(d).is_err() {
            eprintln!("{}", json!({"event": "webhook.dropped", "why": "the queue is full"}));
        }
    }
}

/// Every attempt until one answers 2xx.
async fn deliver(d: &Delivery) {
    // Bounded by ATTEMPTS.
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(BACKOFF[(attempt - 1) as usize]).await;
        }
        let t = macrofiche_contract::time::now_s();
        let sig = signature(&d.body, &d.secret, t);
        match tokio::time::timeout(ATTEMPT_TIMEOUT, post(&d.url, &sig, d.body.clone())).await {
            Ok(Ok(s)) if (200..300).contains(&s) => return,
            Ok(Ok(s)) => eprintln!("{}", json!({"event": "webhook.refused", "url": d.url, "status": s, "attempt": attempt + 1})),
            Ok(Err(e)) => eprintln!("{}", json!({"event": "webhook.failed", "url": d.url, "error": e, "attempt": attempt + 1})),
            Err(_) => eprintln!("{}", json!({"event": "webhook.timeout", "url": d.url, "attempt": attempt + 1})),
        }
    }
    eprintln!("{}", json!({"event": "webhook.gave_up", "url": d.url, "attempts": ATTEMPTS}));
}

/// One POST over HTTP/1.1, http or https; its status.
async fn post(url: &str, sig: &str, body: Vec<u8>) -> Result<u16, String> {
    let uri: hyper::Uri = url.parse().map_err(|e| format!("{e}"))?;
    let host = uri.host().ok_or("no host")?.to_string();
    let https = uri.scheme_str() == Some("https");
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|e| e.to_string())?;
    let req = hyper::Request::post(path)
        .header("host", if (https && port == 443) || (!https && port == 80) { host.clone() } else { format!("{host}:{port}") })
        .header("content-type", "application/json")
        .header("user-agent", "Pierre-Webhook/1.0")
        .header("x-pierre-event", "push")
        .header("x-pierre-signature", sig)
        .body(http_body_util::Full::new(Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    if https {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(host).map_err(|e| e.to_string())?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| e.to_string())?;
        send(hyper_util::rt::TokioIo::new(tls), req).await
    } else {
        send(hyper_util::rt::TokioIo::new(tcp), req).await
    }
}

async fn send<T>(io: T, req: hyper::Request<http_body_util::Full<Bytes>>) -> Result<u16, String>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    // Read (and drop) the answer, bounded, so the connection ends cleanly.
    let _ = http_body_util::Limited::new(resp.into_body(), 64 * 1024).collect().await;
    Ok(status)
}

/// The deliveries for one move: the org's hooks and the repository's own.
pub fn deliveries(org_hooks: &[OrgHook], repo_hooks: &[OrgHook], body: &[u8]) -> Vec<Delivery> {
    org_hooks.iter().chain(repo_hooks).map(|h| Delivery { url: h.url.clone(), secret: h.secret.clone(), body: body.to_vec() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: a delivery is signed as code.storage signs one, so the cell's
    // verifier holds macrofiche to the real format. Method: the contract's
    // test vector (section 6, from the fake's own test, computed by
    // OpenSSL).
    #[test]
    fn signed_as_code_storage_signs() {
        let body = br#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
        assert_eq!(signature(body, b"s3cret", 1_790_000_000), "t=1790000000,sha256=4d38e01ea6039c2094e98395081b85c0efd6af8ae2740ebfab403ded420cb4b8");
    }

    // Goal: the body carries what the cell reads (ref, before, after,
    // repository.url, pushed_at) and the service's fields. Method: one
    // body, read back.
    #[test]
    fn the_body_is_the_union() {
        let b = body(&Push { org: "fragment-dev", repo_id: "abc", repo_name: "todo--paul", ref_name: "refs/heads/main", before: "0".repeat(40), after: "1".repeat(40), pushed_at: "2026-10-03T12:00:00Z".into() });
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ref"], "refs/heads/main");
        assert_eq!(v["repository"]["url"], "todo--paul");
        assert_eq!(v["repository"]["repo_name"], "todo--paul");
        assert_eq!(v["repository"]["id"], "abc");
        assert_eq!((v["type"].as_str(), v["org"].as_str(), v["pushed_at"].as_str()), (Some("push"), Some("fragment-dev"), Some("2026-10-03T12:00:00Z")));
        assert_eq!(v["before"], "0".repeat(40));
    }
}
