//! macrofiche's tooling (`cargo xtask <command>`).
//!
//! - `engine [--tarball <path>] [--prefix <dir>]`: git 2.55.0 into the
//!   prefix (default `target/engine/git-2.55.0`). The release tarball is
//!   downloaded from kernel.org's mirror (or taken from `--tarball`), its
//!   SHA-256 checked against the pin (`macrofiche_git::pin`) before it is
//!   unpacked, and built with `make` and the pin's flags, the environment
//!   cleared but for `PATH`. The prefix gets `macrofiche-engine.json`, the
//!   build's record, and `git --version` is checked.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use http_body_util::BodyExt;
use macrofiche_git::pin;
use thiserror::Error;

#[derive(Debug, Error)]
enum XtaskError {
    #[error("{what}: {source}")]
    Io { what: String, source: std::io::Error },
    #[error("download: {0}")]
    Download(String),
    #[error("the tarball's SHA-256 is {got}, the pin says {want}")]
    Checksum { got: String, want: String },
    #[error("{0}")]
    Build(String),
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> XtaskError {
    let what = what.into();
    move |source| XtaskError::Io { what, source }
}

fn usage() -> ExitCode {
    eprintln!("usage: cargo xtask engine [--tarball <path>] [--prefix <dir>]");
    ExitCode::from(2)
}

fn workspace() -> PathBuf {
    // crates/xtask → the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("crates/xtask sits two below the root").to_path_buf()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    match args.first().map(String::as_str) {
        Some("engine") => {
            let target = workspace().join("target");
            let prefix = flag("--prefix").map(PathBuf::from).unwrap_or_else(|| target.join(pin::PREFIX_DIR));
            match engine(&target.join("engine"), &prefix, flag("--tarball").map(PathBuf::from)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xtask engine: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => usage(),
    }
}

fn sha256_file(path: &Path) -> Result<String, XtaskError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(io(format!("opening {}", path.display())))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    // Bounded by the file's length.
    loop {
        let n = f.read(&mut buf).map_err(io(format!("reading {}", path.display())))?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn engine(work: &Path, prefix: &Path, tarball: Option<PathBuf>) -> Result<(), XtaskError> {
    if !prefix.is_absolute() {
        return Err(XtaskError::Build(format!("--prefix must be absolute: {}", prefix.display())));
    }
    std::fs::create_dir_all(work).map_err(io(format!("creating {}", work.display())))?;
    let tarball = match tarball {
        Some(t) => t,
        None => {
            let t = work.join(pin::TARBALL_NAME);
            let fresh = t.is_file() && sha256_file(&t)? == pin::TARBALL_SHA256;
            if !fresh {
                eprintln!("xtask engine: downloading {}", pin::TARBALL_URL);
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(io("a runtime"))?;
                rt.block_on(download(pin::TARBALL_URL, &t))?;
            }
            t
        }
    };
    let got = sha256_file(&tarball)?;
    if got != pin::TARBALL_SHA256 {
        return Err(XtaskError::Checksum { got, want: pin::TARBALL_SHA256.into() });
    }
    eprintln!("xtask engine: {} checked ({})", tarball.display(), &got[..16]);

    let src_root = work.join("src");
    let _ = std::fs::remove_dir_all(&src_root);
    std::fs::create_dir_all(&src_root).map_err(io(format!("creating {}", src_root.display())))?;
    let f = std::fs::File::open(&tarball).map_err(io(format!("opening {}", tarball.display())))?;
    tar::Archive::new(flate2::read::GzDecoder::new(f)).unpack(&src_root).map_err(io("unpacking the tarball"))?;
    let src = src_root.join(pin::SOURCE_DIR);
    if !src.join("Makefile").is_file() {
        return Err(XtaskError::Build(format!("no Makefile in {}", src.display())));
    }

    let _ = std::fs::remove_dir_all(prefix);
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let log_path = work.join("build.log");
    let log = std::fs::File::create(&log_path).map_err(io(format!("creating {}", log_path.display())))?;
    let mut make = std::process::Command::new("make");
    make.current_dir(&src)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .arg(format!("-j{jobs}"))
        .arg(format!("prefix={}", prefix.display()))
        .args(pin::MAKE_FLAGS)
        .arg("install")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(io("the build log"))?)
        .stderr(log);
    eprintln!("xtask engine: make -j{jobs} install into {} (log: {})", prefix.display(), log_path.display());
    let t0 = std::time::Instant::now();
    let status = make.status().map_err(io("running make"))?;
    if !status.success() {
        return Err(XtaskError::Build(format!("make exited {status}; see {}", log_path.display())));
    }
    let build_s = t0.elapsed().as_secs_f64();

    let git = prefix.join("bin/git");
    let out = std::process::Command::new(&git).env_clear().arg("--version").output().map_err(io(format!("running {}", git.display())))?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line != pin::VERSION_LINE {
        return Err(XtaskError::Build(format!("{} --version says {line:?}, not {:?}", git.display(), pin::VERSION_LINE)));
    }
    let record = serde_json::json!({
        "version": pin::VERSION,
        "tarball_url": pin::TARBALL_URL,
        "tarball_sha256": pin::TARBALL_SHA256,
        "make_flags": pin::MAKE_FLAGS,
        "prefix": prefix,
        "build_s": (build_s * 10.0).round() / 10.0,
    });
    let record_path = prefix.join("macrofiche-engine.json");
    let mut f = std::fs::File::create(&record_path).map_err(io(format!("creating {}", record_path.display())))?;
    f.write_all(serde_json::to_string_pretty(&record).expect("a record serializes").as_bytes()).map_err(io("writing the record"))?;
    println!("{record}");
    Ok(())
}

/// One HTTPS GET into `dest`, bounded by the pin's size, over HTTP/1.1.
async fn download(url: &str, dest: &Path) -> Result<(), XtaskError> {
    use hyper::body::Bytes;
    let d = |e: &dyn std::fmt::Display| XtaskError::Download(e.to_string());
    let uri: hyper::Uri = url.parse().map_err(|e| d(&e))?;
    let host = uri.host().ok_or_else(|| XtaskError::Download("no host".into()))?.to_string();
    let tcp = tokio::net::TcpStream::connect((host.as_str(), 443)).await.map_err(|e| d(&e))?;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| d(&e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| d(&e))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| d(&e))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls)).await.map_err(|e| d(&e))?;
    tokio::spawn(conn);
    let req = hyper::Request::get(uri.path())
        .header("host", &host)
        .header("user-agent", "macrofiche-xtask")
        .body(http_body_util::Empty::<Bytes>::new())
        .map_err(|e| d(&e))?;
    let resp = sender.send_request(req).await.map_err(|e| d(&e))?;
    if resp.status() != 200 {
        return Err(XtaskError::Download(format!("{url}: {}", resp.status())));
    }
    let tmp = dest.with_extension("part");
    let mut f = std::fs::File::create(&tmp).map_err(io(format!("creating {}", tmp.display())))?;
    let mut body = resp.into_body();
    let mut total = 0u64;
    // Bounded by TARBALL_BYTES_MAX.
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| d(&e))?;
        if let Ok(data) = frame.into_data() {
            total += data.len() as u64;
            if total > pin::TARBALL_BYTES_MAX {
                return Err(XtaskError::Download(format!("{url} passes {} bytes", pin::TARBALL_BYTES_MAX)));
            }
            f.write_all(&data).map_err(io("writing the tarball"))?;
        }
    }
    f.sync_all().map_err(io("syncing the tarball"))?;
    std::fs::rename(&tmp, dest).map_err(io(format!("renaming to {}", dest.display())))?;
    Ok(())
}
