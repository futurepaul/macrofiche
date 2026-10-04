//! macrofiche: code.storage's API on git (docs/contract.md,
//! docs/design.md). The config (`config`), the routes (`api`), the push
//! webhook (`webhook`), and the server's life (`serve`): boot recovery
//! before the first request, idle maintenance while it runs, a graceful
//! stop on SIGTERM.

pub mod api;
pub mod config;
pub mod smart;
pub mod webhook;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use macrofiche_git::jail::Settings;
use macrofiche_store::Store;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::api::App;
use crate::config::{Config, JailMode};

/// Connections open at once; past it, a new one is closed at once.
pub const CONNECTIONS_MAX: usize = 4096;
/// A request's headers must arrive within this.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// A TLS handshake must finish within this.
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// After SIGTERM, how long requests in flight have to finish.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);
/// How often maintenance looks for idle repositories, and idle readers.
pub const MAINTENANCE_EVERY: Duration = Duration::from_secs(15);

fn tls_acceptor(c: &config::TlsConfig) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(&c.cert_file).map_err(|e| format!("{}: {e}", c.cert_file.display()))?.collect::<Result<Vec<_>, _>>().map_err(|e| format!("{}: {e}", c.cert_file.display()))?;
    let key = PrivateKeyDer::from_pem_file(&c.key_file).map_err(|e| format!("{}: {e}", c.key_file.display()))?;
    let mut sc = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls: {e}"))?;
    sc.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(sc)))
}

async fn serve_conn<I>(app: Arc<App>, io: I)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| api::handle(app.clone(), req));
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        // A client that hangs up once its request is sent keeps its
        // request: a whole commit pack lands though its answer is lost
        // (contract section 9), as the store's writes run to their end.
        .half_close(true)
        .keep_alive(true)
        .serve_connection(io, svc)
        .await;
}

/// Serves `config` until SIGTERM or SIGINT. `exe` is this binary's path
/// (the engine is found beside it unless the config names one).
pub async fn serve(config: Config, exe: PathBuf) -> Result<(), String> {
    let keyring = config.keyring().map_err(|e| e.to_string())?;
    let org_hooks: HashMap<String, Vec<config::OrgHook>> = config.hooks().map_err(|e| e.to_string())?.into_iter().collect();
    let mut settings = Settings::linux(config.git_prefix(&exe));
    settings.enforce = config.jail == JailMode::Enforce;
    let tls = config.tls.as_ref().map(tls_acceptor).transpose()?;
    let (store, recovery) = Store::open(config.state_dir.clone(), settings).await.map_err(|e| e.to_string())?;
    eprintln!("{}", json!({ "event": "recovered", "recovery": recovery }));
    let app = Arc::new(App {
        store: store.clone(),
        keyring,
        org_hooks,
        hooks: webhook::Hooks::start(),
        requests: Arc::new(Semaphore::new(api::REQUESTS_INFLIGHT_MAX)),
        packs: Arc::new(Semaphore::new(api::PACKS_INFLIGHT_MAX)),
    });
    let listener = tokio::net::TcpListener::bind(&config.listen).await.map_err(|e| format!("{}: {e}", config.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    // The line a supervisor (or a test) waits for: serving, and where.
    println!("{}", json!({ "event": "serving", "addr": addr.to_string(), "tls": tls.is_some(), "landlock_abi": store.engine().landlock_abi(), "pid": std::process::id() }));
    let idle = Duration::from_secs(config.maintenance_idle_s.unwrap_or(macrofiche_store::MAINTENANCE_IDLE.as_secs()));
    let maint = store.clone();
    tokio::spawn(async move {
        // Unbounded by design: maintenance runs for the process's life.
        loop {
            tokio::time::sleep(MAINTENANCE_EVERY).await;
            for m in maint.maintain(idle).await {
                eprintln!("{}", json!({ "event": "maintained", "repo": m }));
            }
            maint.reap_idle();
        }
    });
    let conns = Arc::new(Semaphore::new(CONNECTIONS_MAX));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map_err(|e| e.to_string())?;
    // Unbounded by design: the server accepts until it is told to stop.
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            accepted = listener.accept() => {
                let Ok((tcp, _)) = accepted else { continue };
                let Ok(slot) = conns.clone().try_acquire_owned() else { continue };
                let _ = tcp.set_nodelay(true);
                let app = app.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    match tls {
                        None => serve_conn(app, hyper_util::rt::TokioIo::new(tcp)).await,
                        Some(acceptor) => {
                            if let Ok(Ok(s)) = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                                serve_conn(app, hyper_util::rt::TokioIo::new(s)).await;
                            }
                        }
                    }
                    drop(slot);
                });
            }
        }
    }
    drop(listener);
    eprintln!("{}", json!({ "event": "stopping", "in_flight": api::REQUESTS_INFLIGHT_MAX - app.requests.available_permits() }));
    // Every request in flight holds a slot; all back means all answered.
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, app.requests.acquire_many(api::REQUESTS_INFLIGHT_MAX as u32)).await;
    Ok(())
}
