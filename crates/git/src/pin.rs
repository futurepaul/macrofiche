//! The engine's pin: which git, from where, checked how, built how. The
//! build is `cargo xtask engine` (crates/xtask); the service checks the
//! version it finds against [`VERSION`] before it serves.

/// The git release macrofiche is built and tested against.
pub const VERSION: &str = "2.55.0";
/// `git --version`'s answer from the pinned build.
pub const VERSION_LINE: &str = "git version 2.55.0";
/// The release tarball, from kernel.org's mirror.
pub const TARBALL_URL: &str = "https://mirrors.edge.kernel.org/pub/software/scm/git/git-2.55.0.tar.gz";
pub const TARBALL_NAME: &str = "git-2.55.0.tar.gz";
/// SHA-256 of the tarball, from kernel.org's signed `sha256sums.asc`
/// (2026-10-03), checked before anything in it is used.
pub const TARBALL_SHA256: &str = "0842dc384a23ac33ba3e570c4f3a8ded85963ee4713b1cd21153c3db41813d1e";
/// The tarball's size bound while downloading.
pub const TARBALL_BYTES_MAX: u64 = 64 << 20;
/// The source directory inside the tarball.
pub const SOURCE_DIR: &str = "git-2.55.0";
/// Where the prefix goes, under the workspace's `target/`.
pub const PREFIX_DIR: &str = "engine/git-2.55.0";

/// `make` variables for the build: no curl (a server fetches nothing),
/// nothing that needs Perl, Python, Tcl/Tk, gettext, expat or OpenSSL
/// (SHA-1 is git's collision-detecting sha1dc), and a runtime prefix so
/// the installed tree can move.
pub const MAKE_FLAGS: &[&str] = &[
    "NO_CURL=1",
    "NO_EXPAT=1",
    "NO_PERL=1",
    "NO_PYTHON=1",
    "NO_TCLTK=1",
    "NO_GETTEXT=1",
    "NO_OPENSSL=1",
    "NO_GITWEB=1",
    "NO_INSTALL_HARDLINKS=1",
    "INSTALL_SYMLINKS=1",
    "SKIP_DASHED_BUILT_INS=1",
    "RUNTIME_PREFIX=YesPlease",
];

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the pin's parts agree with one another. Method: each names the
    // same version.
    #[test]
    fn the_pin_agrees_with_itself() {
        assert!(VERSION_LINE.ends_with(VERSION));
        assert!(TARBALL_URL.ends_with(TARBALL_NAME));
        assert_eq!(TARBALL_NAME, format!("git-{VERSION}.tar.gz"));
        assert_eq!(SOURCE_DIR, format!("git-{VERSION}"));
        assert!(PREFIX_DIR.ends_with(SOURCE_DIR));
        assert_eq!(TARBALL_SHA256.len(), 64);
        assert!(TARBALL_SHA256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert!(MAKE_FLAGS.contains(&"NO_CURL=1"));
    }
}
