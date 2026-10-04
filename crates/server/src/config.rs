//! macrofiche's config file (docs/design.md, Config), read once at start
//! and checked. JSON:
//!
//! ```json
//! {"listen": "127.0.0.1:9600",
//!  "state_dir": "/var/lib/macrofiche",
//!  "orgs": [{"name": "fragment-dev",
//!            "keys": [{"pem_file": "/etc/macrofiche/fragment-dev.pub.pem", "kid": "k1"}],
//!            "webhooks": [{"url": "https://…", "secret_file": "/etc/macrofiche/hook.secret"}]}],
//!  "git_prefix": "/opt/macrofiche/git-2.55.0",
//!  "jail": "enforce",
//!  "tls": {"cert_file": "…", "key_file": "…"},
//!  "maintenance_idle_s": 60}
//! ```
//!
//! Only `listen`, `state_dir` and `orgs` (each with a key) are required:
//! fragment's dev stack writes just those (its `crates/devstack/src/
//! codestore.rs`). A key is SPKI PEM, by file (`pem_file`) or inline
//! (`pem`); secrets are only ever files.

use std::path::{Path, PathBuf};

use macrofiche_auth::{keys, Keyring, PublicKey};
use macrofiche_contract::limits::{KEYS_PER_ORG_MAX, ORGS_MAX, WEBHOOKS_PER_ORG_MAX};
use macrofiche_contract::names;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("config: {0}")]
pub struct ConfigError(pub String);

fn err(s: impl Into<String>) -> ConfigError {
    ConfigError(s.into())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default)]
    pub pem_file: Option<PathBuf>,
    #[serde(default)]
    pub pem: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    pub url: String,
    pub secret_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgConfig {
    pub name: String,
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub webhooks: Vec<HookConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JailMode {
    #[default]
    Enforce,
    /// For a host without Landlock, by the operator's explicit choice.
    Off,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub state_dir: PathBuf,
    pub orgs: Vec<OrgConfig>,
    #[serde(default)]
    pub git_prefix: Option<PathBuf>,
    #[serde(default)]
    pub jail: JailMode,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub maintenance_idle_s: Option<u64>,
}

/// An org's webhook, its secret read.
#[derive(Clone, Debug)]
pub struct OrgHook {
    pub url: String,
    pub secret: Vec<u8>,
}

/// Bytes of a webhook secret's file.
pub const SECRET_BYTES_MAX: usize = 4096;
/// The longest idle wait before maintenance a config may set.
pub const MAINTENANCE_IDLE_S_MAX: u64 = 24 * 3600;

/// Whether a webhook URL is one macrofiche can deliver to.
pub fn valid_hook_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() <= 2048 && u.parse::<hyper::Uri>().is_ok_and(|uri| uri.host().is_some())
}

impl Config {
    pub fn read(path: &Path) -> Result<Config, ConfigError> {
        let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        let c: Config = serde_json::from_slice(&bytes).map_err(|e| err(format!("{}: {e}", path.display())))?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.listen.parse::<std::net::SocketAddr>().map_err(|_| err(format!("listen: an address and port, not {:?}", self.listen)))?;
        if !self.state_dir.is_absolute() {
            return Err(err("state_dir: an absolute path"));
        }
        if self.orgs.is_empty() || self.orgs.len() > ORGS_MAX {
            return Err(err(format!("orgs: 1 to {ORGS_MAX}")));
        }
        let mut seen = Vec::new();
        for o in &self.orgs {
            if !names::valid_org(&o.name) || seen.contains(&&o.name) {
                return Err(err(format!("orgs: {:?} is no org name, or appears twice", o.name)));
            }
            seen.push(&o.name);
            if o.keys.is_empty() || o.keys.len() > KEYS_PER_ORG_MAX {
                return Err(err(format!("{}: 1 to {KEYS_PER_ORG_MAX} keys", o.name)));
            }
            for k in &o.keys {
                if k.pem_file.is_some() == k.pem.is_some() {
                    return Err(err(format!("{}: a key is pem_file or pem, one of them", o.name)));
                }
            }
            if o.webhooks.len() > WEBHOOKS_PER_ORG_MAX {
                return Err(err(format!("{}: at most {WEBHOOKS_PER_ORG_MAX} webhooks", o.name)));
            }
            for h in &o.webhooks {
                if !valid_hook_url(&h.url) {
                    return Err(err(format!("{}: webhook url {:?} is no http(s) URL", o.name, h.url)));
                }
            }
        }
        if self.maintenance_idle_s.is_some_and(|s| s > MAINTENANCE_IDLE_S_MAX) {
            return Err(err(format!("maintenance_idle_s: at most {MAINTENANCE_IDLE_S_MAX}")));
        }
        Ok(())
    }

    /// Every org's keys, read and parsed.
    pub fn keyring(&self) -> Result<Keyring, ConfigError> {
        let mut ring = Keyring::default();
        for o in &self.orgs {
            let mut keys = Vec::new();
            for k in &o.keys {
                let pem = match (&k.pem_file, &k.pem) {
                    (Some(f), None) => std::fs::read_to_string(f).map_err(|e| err(format!("{}: {}: {e}", o.name, f.display())))?,
                    (None, Some(p)) => p.clone(),
                    _ => unreachable!("validated"),
                };
                let point = keys::spki_point(&pem).map_err(|e| err(format!("{}: {e}", o.name)))?;
                keys.push(PublicKey { kid: k.kid.clone(), point });
            }
            ring.add(&o.name, keys);
        }
        Ok(ring)
    }

    /// Every org's webhooks, their secrets read from their files.
    pub fn hooks(&self) -> Result<Vec<(String, Vec<OrgHook>)>, ConfigError> {
        let mut out = Vec::new();
        for o in &self.orgs {
            let mut hooks = Vec::new();
            for h in &o.webhooks {
                let secret = std::fs::read(&h.secret_file).map_err(|e| err(format!("{}: {}: {e}", o.name, h.secret_file.display())))?;
                let secret = secret.trim_ascii().to_vec();
                if secret.is_empty() || secret.len() > SECRET_BYTES_MAX {
                    return Err(err(format!("{}: a webhook secret of 1 to {SECRET_BYTES_MAX} bytes", o.name)));
                }
                hooks.push(OrgHook { url: h.url.clone(), secret });
            }
            out.push((o.name.clone(), hooks));
        }
        Ok(out)
    }

    /// The git prefix: the config's, else `MACROFICHE_GIT_PREFIX`, else
    /// beside the binary as `cargo xtask engine` builds it
    /// (`target/release/macrofiche` → `target/engine/git-2.55.0`).
    pub fn git_prefix(&self, exe: &Path) -> PathBuf {
        if let Some(p) = &self.git_prefix {
            return p.clone();
        }
        if let Some(p) = std::env::var_os("MACROFICHE_GIT_PREFIX") {
            return PathBuf::from(p);
        }
        exe.parent().and_then(Path::parent).map(|t| t.join(macrofiche_git::pin::PREFIX_DIR)).unwrap_or_else(|| PathBuf::from(macrofiche_git::pin::PREFIX_DIR))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the config fragment's dev stack writes is read as it is
    // (crates/devstack/src/codestore.rs, `macrofiche_config`). Method:
    // its JSON, word for word.
    #[test]
    fn the_dev_stacks_config_is_read() {
        let c: Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:9101",
            "state_dir": "/tmp/mf/state",
            "orgs": [{ "name": "fragment-e2e", "keys": [{ "pem_file": "/tmp/mf/fragment-e2e.pub.pem" }] }],
        }))
        .unwrap();
        c.validate().unwrap();
        assert_eq!((c.jail, c.git_prefix.is_none(), c.orgs[0].webhooks.len()), (JailMode::Enforce, true, 0));
        assert_eq!(c.git_prefix(Path::new("/w/target/release/macrofiche")), PathBuf::from("/w/target/engine/git-2.55.0"));
    }

    // Goal: a config that would serve wrongly is refused at start.
    // Method: one fault each.
    #[test]
    fn faults_are_refused() {
        let base = || serde_json::json!({"listen": "127.0.0.1:9600", "state_dir": "/s", "orgs": [{"name": "o", "keys": [{"pem": "x"}]}]});
        let bad = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base();
            f(&mut v);
            serde_json::from_value::<Config>(v).map_err(|e| err(e.to_string())).and_then(|c| c.validate()).is_err()
        };
        assert!(!bad(&|_| {}));
        assert!(bad(&|v| v["listen"] = "nowhere".into()));
        assert!(bad(&|v| v["state_dir"] = "relative".into()));
        assert!(bad(&|v| v["orgs"] = serde_json::json!([])));
        assert!(bad(&|v| v["orgs"][0]["name"] = "a b".into()));
        assert!(bad(&|v| v["orgs"][0]["keys"] = serde_json::json!([])));
        assert!(bad(&|v| v["orgs"][0]["keys"][0]["pem_file"] = "/k".into()), "pem and pem_file both");
        assert!(bad(&|v| v["orgs"][0]["webhooks"] = serde_json::json!([{"url": "ftp://x", "secret_file": "/s"}])));
        assert!(bad(&|v| v["surprise"] = true.into()), "unknown fields are refused, not ignored");
        assert!(bad(&|v| v["orgs"].as_array_mut().unwrap().push(serde_json::json!({"name": "o", "keys": [{"pem": "x"}]}))), "an org twice");
    }
}
