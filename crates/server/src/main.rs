//! `macrofiche`: code.storage's API on git.
//!
//! - `serve --config <path>`: serve (docs/design.md, Config), printing one
//!   JSON line, `{"event":"serving","addr":…}`, to stdout once it answers.
//! - `check --config <path>`: read and check a config, its keys and
//!   secrets, and print what it serves.
//! - `pubkey <pkcs8.pem>`: the SPKI PEM of a P-256 private key's public
//!   half (fragment's `CODESTORAGE_PRIVATE_KEY`), for a config's keys.
//! - `version`: macrofiche's version and the git it is pinned to.

use std::path::PathBuf;
use std::process::ExitCode;

use macrofiche::config::Config;

fn usage() -> ExitCode {
    eprintln!("usage: macrofiche serve --config <path> | check --config <path> | pubkey <private-key.pem> | version");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("macrofiche"));
    match args.first().map(String::as_str) {
        Some("serve") => {
            let Some(path) = flag("--config") else { return usage() };
            let config = match Config::read(path.as_ref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("macrofiche: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("macrofiche: a runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match rt.block_on(macrofiche::serve(config, exe)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("macrofiche: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("check") => {
            let Some(path) = flag("--config") else { return usage() };
            let checked = Config::read(path.as_ref()).and_then(|c| {
                c.keyring()?;
                let hooks = c.hooks()?;
                Ok(serde_json::json!({
                    "listen": c.listen,
                    "state_dir": c.state_dir,
                    "git_prefix": c.git_prefix(&exe),
                    "orgs": c.orgs.iter().map(|o| serde_json::json!({ "name": o.name, "keys": o.keys.len() })).collect::<Vec<_>>(),
                    "webhooks": hooks.iter().map(|(_, h)| h.len()).sum::<usize>(),
                }))
            });
            match checked {
                Ok(v) => {
                    println!("{v}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("macrofiche: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("pubkey") => {
            let Some(path) = args.get(1) else { return usage() };
            match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|pem| macrofiche_auth::keys::public_of_pkcs8(&pem).map_err(|e| e.to_string())) {
                Ok(public) => {
                    print!("{public}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("macrofiche: {path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("version") => {
            println!("macrofiche {} (git {})", env!("CARGO_PKG_VERSION"), macrofiche_git::pin::VERSION);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
