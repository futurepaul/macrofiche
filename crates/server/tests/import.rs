//! `macrofiche import` (docs/design.md, Import), through the binary: a
//! repository from another store's fast-import stream, then served under
//! the url form and the commit ids that store's clients hold, as
//! fragment's cell holds them; and an import beside a running macrofiche,
//! refused.

mod common;

use std::process::Command;

use common::{bin, config, Fx, ORG};
use macrofiche_testkit::fragment::{commit_pack, committed, up};
use macrofiche_testkit::{Key, Macrofiche};
use serde_json::json;

/// The url form fragment's code.storage fake names a repository by.
const URL: &str = "0f3c2a1e-5b6d-4e7f-8a9b-0c1d2e3f4a5b";
const OLD_ONE: &str = "1111111111111111111111111111111111111111";
const OLD_TWO: &str = "2222222222222222222222222222222222222222";

/// Two commits on their legacy ids' refs, main at the second, live at the first.
fn stream() -> String {
    let mut s = String::new();
    s.push_str("blob\nmark :1\ndata 5\nalpha\nblob\nmark :2\ndata 4\nbeta\n");
    s.push_str(&format!("commit refs/macrofiche/legacy/{OLD_ONE}\nmark :3\nauthor e2e <e2e@users.fragment> 1000 +0000\ncommitter e2e <e2e@users.fragment> 1000 +0000\ndata 3\none\nM 100644 :1 a.txt\n\n"));
    s.push_str(&format!("commit refs/macrofiche/legacy/{OLD_TWO}\nmark :4\nauthor e2e <e2e@users.fragment> 2000 +0000\ncommitter e2e <e2e@users.fragment> 2000 +0000\ndata 3\ntwo\nfrom :3\nM 100644 :2 a.txt\n\n"));
    s.push_str("reset refs/heads/main\nfrom :4\n\nreset refs/heads/live\nfrom :3\n\ndone\n");
    s
}

fn import(cfg: &std::path::Path, name: &str, url: &str, id: &str, stream: &std::path::Path) -> std::process::Output {
    Command::new(bin())
        .args(["import", "--config"])
        .arg(cfg)
        .args(["--org", ORG, "--name", name, "--url", url, "--repo-id", id, "--created-at-ms", "1700000000000"])
        .arg(stream)
        .output()
        .expect("macrofiche import runs")
}

// Goal: a repository imported while nothing serves is then served as its
// old store served it: by its url form (its name is no url), listed and
// looked up by id under it, read at an old commit id, written with a
// compare-and-swap on one, and deployed by a merge expecting one; and an
// import while macrofiche serves the same state is refused, changing
// nothing. Method: the binary's import, then its serve, called as the
// cell calls it.
#[test]
fn an_imported_repository_is_served_by_its_old_names() {
    let dir = macrofiche_testkit::scratch("import-cli");
    let key = Key::generate();
    let mut c = config(&dir, &key, json!({}));
    c["listen"] = "127.0.0.1:0".into();
    c["state_dir"] = dir.join("state").to_string_lossy().into_owned().into();
    c["git_prefix"] = macrofiche_testkit::git_prefix().to_string_lossy().into_owned().into();
    let cfg = dir.join("macrofiche.json");
    std::fs::write(&cfg, serde_json::to_vec_pretty(&c).unwrap()).unwrap();
    let s = dir.join("todo.fi");
    std::fs::write(&s, stream()).unwrap();

    let out = import(&cfg, "todo--abc", URL, "repo_0123", &s);
    assert!(out.status.success(), "import: {}", String::from_utf8_lossy(&out.stderr));
    let said: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((said["event"].as_str(), said["imported"]["legacy"].as_u64()), (Some("imported"), Some(2)), "{said}");

    let fx = Fx { m: Macrofiche::restart(&bin(), &dir), key, dir: dir.clone() };
    let main = fx.head(URL, "main").expect("main, by the url form");
    assert_eq!(fx.get("todo--abc", "branch", "name=main").status, 404, "its name is no url form of it");
    let listed = fx.call("GET", "/api/repos?limit=10", ORG, &["org:read"], None);
    assert_eq!((listed.json()["repos"][0]["url"].as_str(), listed.json()["repos"][0]["repo_name"].as_str()), (Some(URL), Some("todo--abc")), "{listed}");
    let by_id = fx.call("GET", "/api/repo-urls/repo_0123", "repo_0123", &["org:read"], None);
    assert_eq!(by_id.json()["url"].as_str(), Some(URL), "{by_id}");
    assert_eq!(fx.read(URL, "a.txt", OLD_TWO).as_deref(), Some(&b"beta"[..]), "read at an old id");
    assert_eq!(fx.read(URL, "a.txt", OLD_ONE).as_deref(), Some(&b"alpha"[..]));
    let listing = fx.list(URL, OLD_TWO, 10);
    assert_eq!(listing, vec![("a.txt".to_string(), 4, main.clone())], "last commits, git's");

    let a = fx.pack(URL, &commit_pack("main", Some(OLD_TWO), "on the old id", ("e2e", "e2e@users.fragment"), &[up("b.txt", b"new")]));
    let new = committed(&a.json()).unwrap_or_else(|| panic!("a write expecting main's old id: {a}"));
    let a = fx.merge_live(URL, OLD_ONE, "deploy");
    assert_eq!((a.status, a.json()["target"]["new_sha"].as_str()), (200, Some(new.as_str())), "a merge expecting live's old id: {a}");

    let again = import(&cfg, "other", "1f3c2a1e", "repo_4567", &s);
    assert!(!again.status.success() && String::from_utf8_lossy(&again.stderr).contains("is held"), "an import beside a running macrofiche: {}", String::from_utf8_lossy(&again.stderr));
    let listed = fx.call("GET", "/api/repos?limit=10", ORG, &["org:read"], None);
    assert_eq!(listed.json()["repos"].as_array().map(Vec::len), Some(1), "nothing more was made: {listed}");
}
