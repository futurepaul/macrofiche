# macrofiche

A self-hosted git store that answers code.storage's API: the files half
of a self-hosted fragment (fragment's `docs/self-host.md`, seam 5), as
sandcastle is its computers half. What it answers is
[docs/contract.md](docs/contract.md): thirteen REST routes and a signed
push webhook, as fragment's cell and CLI call them. What it is built on
is [docs/library.md](docs/library.md): git itself, at a pinned version,
driven through its plumbing as jailed processes, the way GitLab's Gitaly
drives git. How the two meet is [docs/design.md](docs/design.md).

## Status

**Design.** The contract and the library decision are done; the design
is written (docs/design.md, phase 1). The engine, the service, and their
evidence are the next phases.

## Layout

| Crate | What |
|---|---|
| `crates/contract` | `macrofiche-contract`: limits, names and their checks, query decoding, cursors, answer and problem bodies (pure) |
| `crates/auth` | `macrofiche-auth`: ES256 JWTs, scopes, orgs and their public keys (pure) |
| `crates/pack` | `macrofiche-pack`: the streaming NDJSON commit-pack parser (pure) |
| `crates/git` | `macrofiche-git`: git 2.55.0 as jailed processes: plumbing calls, the `cat-file` pool, classified errors |
| `crates/store` | `macrofiche-store`: repositories on disk, writes under compare-and-swap, merges, listings, the last-commit index, recovery, maintenance |
| `crates/server` | `macrofiche`: the binary: config, the HTTP API, webhooks |
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
```

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
