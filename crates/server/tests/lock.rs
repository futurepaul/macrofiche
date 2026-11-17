//! One process at a time on a state directory (docs/design.md, Recovery
//! and maintenance), through the binary.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{bin, Fx};
use macrofiche_testkit::Macrofiche;

/// How long a refused second process has to say so and exit.
const REFUSED_WITHIN: Duration = Duration::from_secs(30);

// Goal: a second macrofiche on a state directory another serves is
// refused before boot's sweep, so the first one's uploads in flight
// (its spools) are untouched; once the first is stopped, a new one
// serves. Method: a serve, a spool as an upload leaves one, a second
// serve of the same config, then a restart.
#[test]
fn a_second_process_on_a_state_directory_is_refused() {
    let fx = Fx::start("lock");
    let spool = fx.dir.join("state/spool/0123456789abcdef");
    std::fs::create_dir_all(&spool).unwrap();
    std::fs::write(spool.join("c0"), b"an upload in flight").unwrap();

    let mut second = Command::new(bin()).arg("serve").arg("--config").arg(&fx.m.config).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let out = second.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let line = BufReader::new(out).lines().next().and_then(Result::ok);
        let _ = tx.send(line);
    });
    let served = rx.recv_timeout(REFUSED_WITHIN).ok().flatten();
    if served.is_some() {
        let _ = second.kill();
    }
    let status = second.wait().unwrap();
    let mut said = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut said).unwrap();
    assert!(served.is_none() && !status.success() && said.contains("is held"), "a second serve: {served:?} {status} {said}");
    assert!(spool.join("c0").exists(), "the first one's upload is untouched");

    let Fx { m, dir, .. } = fx;
    assert!(m.stop().success());
    let again = Macrofiche::restart(&bin(), &dir);
    assert!(!spool.exists(), "once the first is gone, the next boot sweeps what it left");
    again.stop();
}
