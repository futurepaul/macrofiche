//! `macrofiche`: code.storage's API on git.
//!
//! - `serve --config <path>`: serve (docs/design.md, Config), printing one
//!   JSON line, `{"event":"serving","addr":…}`, to stdout once it answers.
//! - `check --config <path>`: read and check a config, its keys and
//!   secrets, and print what it serves.
//! - `pubkey <pkcs8.pem>`: the SPKI PEM of a P-256 private key's public
//!   half (fragment's `CODESTORAGE_PRIVATE_KEY`), for a config's keys.
//! - `import --config <path> --org <org> --name <name> [--url <url form>]
//!   [--repo-id <id>] [--created-at-ms <ms>] [--default-branch <b>] <stream>`:
//!   a repository made elsewhere, from a fast-import stream, while nothing
//!   serves the state (docs/design.md, Import), printing what it made.
//! - `version`: macrofiche's version and the git it is pinned to.

use std::path::PathBuf;
use std::process::ExitCode;

use macrofiche::config::Config;

fn usage() -> ExitCode {
    eprintln!("usage: macrofiche serve --config <path> | check --config <path> | pubkey <private-key.pem> | import --config <path> --org <org> --name <name> [--url <url>] [--repo-id <id>] [--created-at-ms <ms>] [--default-branch <b>] <stream> | version");
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
        Some("import") => {
            let (Some(path), Some(org), Some(name), Some(stream)) = (flag("--config"), flag("--org"), flag("--name"), args.last().filter(|a| !a.starts_with("--")).cloned()) else { return usage() };
            let created_at_ms = match flag("--created-at-ms").map(|v| v.parse::<i64>()) {
                None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0),
                Some(Ok(ms)) if ms >= 0 => ms,
                Some(_) => return usage(),
            };
            let (url, repo_id, branch) = (flag("--url"), flag("--repo-id"), flag("--default-branch").unwrap_or_else(|| "main".into()));
            let config = match Config::read(path.as_ref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("macrofiche: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let req = macrofiche_store::Import { org: &org, name: &name, url: url.as_deref(), repo_id: repo_id.as_deref(), default_branch: &branch, created_at_ms, stream: stream.as_ref() };
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("macrofiche: a runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match rt.block_on(macrofiche::import(config, exe, &req)) {
                Ok(made) => {
                    println!("{}", serde_json::json!({ "event": "imported", "imported": made }));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("macrofiche: import {name}: {e}");
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
