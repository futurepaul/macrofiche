# macrofiche

A self-hosted git store that answers code.storage's API: the files half
of a self-hosted fragment (fragment's `docs/self-host.md`, seam 5), as
sandcastle is its computers half. What it answers is
[docs/contract.md](docs/contract.md): fourteen REST routes and a signed
push webhook, as fragment's cell and CLI call them; it also serves git's
smart HTTP (`git clone`, `fetch`, `push`) with the same tokens. What it
is built on is [docs/library.md](docs/library.md): git itself, at a
pinned version, driven through its plumbing as jailed processes, the way
GitLab's Gitaly drives git. How the two meet is
[docs/design.md](docs/design.md).

## Status

**Serving, measured on one machine, not yet run in production.** Every
route of the contract and the push webhook, on git 2.55.0 jailed per
process (docs/design.md; evidence in `docs/evidence/`, measured
2026-10-03 on a Ryzen 9 9950X3D, Linux 7.2.5, btrfs on NVMe):

| | |
|---|---|
| fragment's own e2e (`codestore`, `create`, `deploy`, `files`, `sync`) against the release binary | 123 passed, 0 failed, 6 skipped (the fake's levers, the browser, webhooks) |
| a durable one-file commit pack; a file read; a listing of 106 files at a new head | 7.3 ms; 0.14 ms; 3.5 ms (p50) |
| a 100 MiB blob in and out | 3.7 s in, 47 ms out; macrofiche's heap at most 51 MiB, git's 29 MiB |
| sixteen writers racing on one branch | 320 of 320 commits, linear, 127 commits/s, 2,601 refused 409s applying nothing |
| twenty rounds of kill -9 mid-write and a restart | `fsck --strict` clean every round, 982 acknowledged commits, none lost |
| the real git client over smart HTTP, with the same JWT | a 1,000-file clone in 30 ms, a push in 15 ms; a stale push refused by the CAS |

What is not done: a node on another filesystem or kernel, a run in
production, and the debt in docs/design.md, Debt.

## Layout

| Crate | What |
|---|---|
| `crates/contract` | `macrofiche-contract`: limits, names and their checks, query decoding, cursors, answer and problem bodies (pure) |
| `crates/auth` | `macrofiche-auth`: ES256 JWTs, scopes, orgs and their public keys (pure) |
| `crates/pack` | `macrofiche-pack`: the streaming NDJSON commit-pack parser (pure) |
| `crates/git` | `macrofiche-git`: git 2.55.0 as jailed processes: plumbing calls, the `cat-file` pool, classified errors |
| `crates/store` | `macrofiche-store`: repositories on disk, writes under compare-and-swap, merges, listings, the last-commit index, recovery, maintenance |
| `crates/server` | `macrofiche`: the binary: config, the HTTP API, smart HTTP, webhooks |
| `crates/testkit` | `macrofiche-testkit`: a blocking HTTP client and server, tokens, the binary spawned for tests |
| `crates/bench` | `macrofiche-bench`: the evidence driver |
| `crates/xtask` | `cargo xtask engine`: builds git 2.55.0 into the prefix |

## Build and test

Rust stable (1.99 tested), on Linux:

```sh
cargo xtask engine            # git 2.55.0 into target/engine/git-2.55.0
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p macrofiche -p macrofiche-bench
target/release/macrofiche-bench all --write   # docs/evidence/*.json
```

Running it: `macrofiche serve --config <file>` (docs/design.md, Config
and command line; `macrofiche check --config <file>` reads one,
`macrofiche pubkey <pkcs8.pem>` derives an org's public key from the
private key fragment holds). It needs Linux with Landlock (5.13; 6.7 for
its TCP rules) and no root.

`cargo xtask engine` downloads git's release tarball from kernel.org,
checks its SHA-256 against the pinned value, and builds it with `make`
(a C compiler, `make`, and zlib's headers are needed). Tests that run
git find it at `MACROFICHE_GIT_PREFIX`, else at
`target/engine/git-2.55.0`.

## Docs

- `docs/contract.md`: code.storage's API as fragment uses it.
- `docs/library.md`: why git itself, and the library spike
  (`spikes/library/`).
- `docs/design.md`: the architecture, the decisions, the phases, their
  evidence (`docs/evidence/`), and the debt.

## License

MIT (`LICENSE`), as fragment and sandcastle. git is GPL-2.0; it is not
vendored here, but built from its release tarball by `cargo xtask
engine`, and shipped beside macrofiche as its own program with its
source.
