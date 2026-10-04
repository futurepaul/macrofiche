# macrofiche: the design

*2026-10-03.* Paul: "Make code.storage into a real self-hosted git store,
make it another sibling project like sandcastle… follow the same
engineering-style as sandcastle… base it on [a] low level library like
libkrun… support all the features we use of code.storage including good
blob support."

What macrofiche answers is docs/contract.md (thirteen routes and a
webhook). What it is built on is docs/library.md: git itself, pinned,
driven as processes, jailed. This document is how the two meet: the
architecture, the decisions, the answers to the contract's open
questions, the phases and their exits, and (as phases close) their
evidence and debt.

## Summary

- **One service, `macrofiche`**: an HTTP/1.1 server (hyper on tokio)
  answering code.storage's REST API as fragment uses it, with ES256 JWTs
  verified against each org's public keys from a config file.
- **git 2.55.0 is the engine.** Built from its release tarball (SHA-256
  pinned and checked) into macrofiche's prefix with no curl, run by
  absolute path with the environment cleared and no system or global
  config. Every git process is jailed without root: Landlock confines it
  to its repository, a seccomp filter and Landlock's network rules keep
  it off the network, `PR_SET_NO_NEW_PRIVS`, rlimits for CPU, memory,
  files and core dumps, a deadline, and death with its request.
- **Writes** stream: a commit pack is parsed line by line and each
  `blob_chunk` is decoded into a spool file per `content_id`; then, under
  the repository's write lock, one `fast-import` writes the objects and
  one `update-ref --stdin` transaction moves the branch (the
  compare-and-swap). Objects and refs are fsynced (`core.fsync=all`)
  before the answer.
- **Reads** stream: long-lived `cat-file --batch-command` processes per
  repository serve blobs of any size with bounded memory
  (`core.bigFileThreshold=1m`).
- **Last commits** come from an index macrofiche writes with every commit
  it makes (git's own definition, TREESAME through merges), so a listing
  costs no history walk.
- **The service's semantics** where the fake and the service differ:
  `url` is the repo's name, statuses and problem bodies are the
  service's, merges are git's three-way merge with 409 `merge_conflict`,
  restore refuses with 412, history is in commit-date order.

## How a request flows

```
client ── HTTP/1.1 (TLS optional) ──▶ macrofiche
            │ route, ES256 JWT, scope, repo claim       (macrofiche-auth)
            │ query and body checks, limits             (macrofiche-contract)
            ▼
          store: a repository by org and name           (macrofiche-store)
            │ reads: cat-file pool, ls-tree, log, last-commit index
            │ writes: pack ─▶ spools ─▶ write lock ─▶ fast-import
            │         ─▶ last-commit record ─▶ update-ref (CAS) ─▶ answer
            ▼
          git 2.55.0, one jailed process per operation  (macrofiche-git)
            │
            ▼ after the answer
          push webhook, signed, retried                 (the server)
```

## Crates

| Crate | What | Pure? |
|---|---|---|
| `crates/contract` | `macrofiche-contract`: every limit as a constant; names (orgs, repos, branches, paths, shas) and their checks; query decoding; cursors; RFC 3339 time; the answer and problem bodies, as types | pure, host-tested |
| `crates/auth` | `macrofiche-auth`: ES256 JWT verification (ring), claims, scopes, the route's rules and the service's statuses for each failure; orgs and their public keys | pure, host-tested |
| `crates/pack` | `macrofiche-pack`: the NDJSON commit-pack parser as a state machine fed bytes as they arrive, one line buffered at a time, chunks decoded and handed to a sink | pure, host-tested |
| `crates/git` | `macrofiche-git`: the engine. The jail (Landlock, seccomp, rlimits), process spawning with deadlines, each plumbing command as a typed call, the `cat-file` pool, and errors classified from exit statuses and stderr | parsers and the jail plan pure; the rest needs the engine |
| `crates/store` | `macrofiche-store`: repositories on disk, the registry, the write lock, commit packs applied, merges, restores, branches, listings, the last-commit index, boot recovery, maintenance | the index format and merge rules pure; the rest needs the engine |
| `crates/server` | `macrofiche`: the binary. Config, the HTTP server and every route, streamed bodies, webhook delivery, (later) smart HTTP | config and webhook signing pure |
| `crates/testkit` | `macrofiche-testkit`: a blocking HTTP/1.1 client and server for tests, key generation and token minting, spawning the real binary | test support |
| `crates/bench` | `macrofiche-bench`: the evidence driver; each scenario's numbers as JSON into `docs/evidence/` | driver |
| `crates/xtask` | `cargo xtask engine`: downloads git 2.55.0, checks its SHA-256, builds and installs it into the prefix | tooling |

Every crate is Rust; the repository has no shell or Python scripts.
Dependencies are pinned exactly and, where sandcastle already pins one,
at sandcastle's version (`Cargo.toml` says why each is there).

## On disk

```
<state_dir>/
  repos/<org>/<repo_id>.git/       a bare repository, created by rename (RENAME_NOREPLACE)
    macrofiche/repo.json           {repo_id, repo_name, org, default_branch, created_at}
    macrofiche/lastcommit/aa/<38>  the last-commit index, one record per commit (like loose objects)
    macrofiche/tmp/                git's TMPDIR, inside the jail
  spool/<request>/<content_id>     a commit pack's decoded blobs while it uploads
  tmp/                             repositories being created
```

A repository's directory is named by its `repo_id` (random, 32 hex),
never by its name, so a name may be anything the contract allows (the
service's names may hold `/`). The registry, name to id and id to name,
is read from every `repo.json` at boot and kept in memory: macrofiche is
the only writer.

Each repository is created with this config (library.md,
Recommendation): `core.fsync=all` and `core.fsyncMethod=fsync`;
`core.bigFileThreshold=1m`; `gc.auto=0`, `maintenance.auto=false`;
`core.logAllRefUpdates=false`; `core.hooksPath` pointing at an empty
directory macrofiche owns; `receive.fsckObjects=true`;
`transfer.hideRefs` for `refs/namespaces` and `refs/macrofiche`;
`core.packedGitLimit` and `core.packedGitWindowSize` low enough for the
jail's memory limit.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Engine | git 2.55.0 as processes (library.md) | the only implementation with every part, durability, and the reference merge |
| How git is built | `cargo xtask engine`: the release tarball (SHA-256 `0842dc38…3d1e`, checked), `make` with `NO_CURL`, `NO_EXPAT`, `NO_PERL`, `NO_PYTHON`, `NO_TCLTK`, `NO_GETTEXT`, `NO_OPENSSL`, `RUNTIME_PREFIX`, into `target/engine/git-2.55.0` | sandcastle's libkrun treatment: built into a prefix, run by path, never installed system-wide |
| Jail | unprivileged: Landlock (raw syscalls, ABI ≥ 4 for TCP rules), a seccomp filter refusing non-`AF_UNIX` sockets, `PR_SET_NO_NEW_PRIVS`, rlimits, `PR_SET_PDEATHSIG`, its own process group | macrofiche runs as a plain user; sandcastle's jail needs root because a VMM needs namespaces, git does not |
| Jail rules prepared | in the parent before `fork`; the child only calls `landlock_restrict_self`, `prctl` and `setrlimit` | nothing allocates between `fork` and `exec` in a threaded process |
| HTTP | hyper 1.11 `http1` on tokio, TLS optional (rustls) | sandcastle's stack |
| Signatures | `ring` (ECDSA P-256 verify, HMAC-SHA256, random ids) | already in sandcastle's tree through rustls; one crypto library |
| Commit time | the server's clock, `+0000` | contract 5.8 |
| Repo identity | `url` = `repo_name`; `repo_id` a random id | the service (question 1) |
| Write order | spool, then lock, then `fast-import`, then the index record (fsynced), then `update-ref` (CAS), then answer | the client's upload never holds the lock; an acknowledged commit always has its record |
| Locks | one `tokio::sync::Mutex` per repository for writes; reads take none | one writer at a time is what the CAS would allow anyway, without retries |
| Paths | any UTF-8 git can store, but no control bytes, no `.`, `..` or `.git` segment, at most 4096 bytes; C-quoted in the fast-import stream | 400 for what git cannot store, and for what text protocols cannot carry safely |
| Modes | `100644` and `100755` | what a commit pack may write |
| Ephemeral refs | `refs/namespaces/ephemeral/refs/heads/<name>`, hidden from transfer, never announced | the service's namespace |
| 404 vs 503 | 404 only for what is absent; any failure to read is 503 | contract 5.4: a passing failure must not read as "absent" |

## The contract's open questions (section 14), answered

Each default keeps fragment working against both the fake and the
service; where the two differ, it follows the service, which fragment
runs against in production. **Paul to confirm** marks the ones that are
a policy choice rather than the service's behaviour.

1. **The url form: `url` = `repo_name`**, and `http_url` too, as the
   service documents them. `repo_id` is a stable random id (32 hex).
   Fragment stores whatever `url` says, so it works. e2e `create`'s
   check of a 36-character, four-hyphen repo is a fake-ism and would
   fail against macrofiche as it would against the service. *Paul to
   confirm* the e2e check is relaxed.
2. **Webhooks: configured per org**, in macrofiche's config file (a URL
   and a secret file), as the service's dashboard makes one org-wide
   subscription. No registration route. Fragment does not use webhooks
   on the hosted fleet today (its pins move by `refresh`, its own
   commits, and the poll), so nothing changes for it. *Paul to confirm*
   whether the cell should later get an org-wide receiving route, or
   macrofiche a per-repo registration extension (`PUT
   /api/repos/{repo}/webhook`); either is small.
3. **Merging into a diverged `live`: git's three-way merge**, as the
   service (settled upstream by fragment PR #127, contract section 15):
   `no_op` when the target holds the source, a fast-forward when it can,
   else `merge-tree --write-tree` from the merge base, 409
   `MergeConflict` with `conflict_type: merge_conflict` when the sides
   conflict and `target_moved` when `expected_target_sha` is stale. The
   merge preview answers the same decision without writing. Restore
   answers 412 for the tip, a non-ancestor, and no change.
4. **History order: commit-date order** over every reachable commit, as
   the service (`git log --date-order`): the tip is always first, and on
   linear history (every branch fragment writes but a deploy after a
   rollback) it equals first parents. `limit` defaults to 20, at most
   100, with an opaque cursor that pins the tip it started from.
5. **Large files: keep the pointers.** Fragment's bytes of 1 MiB or more
   stay in its bucket, and a pointer is an ordinary blob to macrofiche.
   macrofiche still holds large blobs well (a blob to 1 GiB, streamed in
   and out, evidence in phase 4), so raising fragment's threshold later
   needs nothing here. A commit pack gets limits (section Limits): 4 MiB
   a chunk (the service's), 1 GiB a blob, 2 GiB decoded a pack, 20 000
   files a pack. **An LFS batch endpoint is not worth adding now**:
   fragment never speaks LFS, its pointers live in its own bucket, and
   git clients that would use LFS arrive only with smart HTTP. It is
   worth adding with smart HTTP if computers' agents push large files
   with `git lfs`; the store would be a directory of objects by SHA-256
   beside the repository, the batch API a few routes. *Paul to confirm.*
6. **Test levers: none in macrofiche.** The e2e runs against macrofiche
   under the hosted lane's rules, which already write through the
   platform's routes and skip the checks that read the fake. The detail
   strings fragment's tests assert (`branch not found`) are kept
   exactly. *Paul to confirm.*
7. **Keys: public keys in the config file** (SPKI PEM, inline or by
   file, an optional `kid` each), as the service stores only public
   keys. `macrofiche pubkey <private.pem>` derives the public half of the
   PKCS#8 key the operator already holds, so nothing secret goes into
   macrofiche's config. Rotation: add the new key, then remove the old.
8. **Unsigned mode: none.** Every request is verified; there is no dev
   mode that accepts any bearer. The CLI's unit tests keep the fake.
   *Paul to confirm.*

## Writes

### A commit pack (5.8)

1. The body is read as it arrives (`Content-Length` or chunked). The
   parser (`macrofiche-pack`) buffers one line at a time (at most 16 MiB
   for the metadata line, 8 MiB for a chunk line), checks the metadata
   (target branch, expected sha, author, files, paths, modes,
   duplicates, content ids), and hands each decoded chunk (at most 4 MiB)
   to the spool of its `content_id`. Chunks may interleave. Nothing
   whole is ever held: one line and one decoded chunk at a time.
2. At the end of the body every upsert must have its `eof`; a chunk for
   an unknown id or a delete is refused; the refusals are the service's
   (400, 413) in the service's body.
3. Under the repository's write lock: read the branch tip; if
   `expected_target_sha` is present and is not the tip, answer 409 at
   once (nothing written). Otherwise run one `fast-import` (`from` the
   tip, blobs inline from the spools, C-quoted paths, `--big-file-threshold=1m`
   so a large blob streams into the pack without a delta search),
   onto a temporary ref `refs/macrofiche/tmp/<id>`.
4. `diff-tree` between the tip and the new commit gives the changed
   paths; the last-commit record is written and fsynced.
5. One `update-ref --stdin` transaction: `update refs/heads/<b> <new>
   <tip>` (or `create`), `delete` of the temporary ref. git's ref
   transaction is the compare-and-swap; it fsyncs the ref.
6. Answer 201; queue the webhook; remove the spools.

A pack whose answer is lost has landed by step 5, so a client that
re-reads sees it (contract section 9).

### Merge (5.10), preview (15.1), restore (5.11), branch create (5.9)

- **Merge**, under the write lock: resolve source and target (404),
  check `expected_target_sha` (409 `target_moved`), then decide with
  `merge-base`: `no_op` (the target holds the source), fast-forward
  (unless `strategy: merge`, which always makes a merge commit; refused
  for `ff_only` when diverged), or `merge-tree --write-tree` (409
  `merge_conflict` with the paths), then `commit-tree` with parents
  `[target, source]` (or `[target]` for `squash`), the index record,
  and the CAS. Unrelated histories are 400 unless
  `allow_unrelated_histories`.
- **Preview**: the same decision, no lock, nothing written:
  `merge-tree --write-tree --name-only` writes objects (an unreferenced
  tree) but no ref; they are pruned by maintenance.
- **Restore**: 404 for an absent target; 412 when `base_ref` is the tip,
  is not an ancestor of it (`merge-base --is-ancestor`), is unknown, or
  has the tip's tree; 409 for a stale `expected_target_sha`; else
  `commit-tree` of `base_ref`'s tree with the tip as its only parent,
  the record, the CAS. 201 in the commit pack's shape.
- **Branch create**: `update-ref --stdin` `create` (one creator wins; a
  second gets 409), in `refs/heads/` or the ephemeral namespace.

## Reads

- **A branch** (5.4): `cat-file --batch-command` `info refs/heads/<b>`
  through the pool (the ref is read fresh each time). 404 `branch not
  found` only when git says the ref is missing; anything else is 503.
- **A file** (5.6, 5.7): resolve the ref to a commit, `info
  <commit>:<path>` for the blob id and size, the last commit from the
  index, then `contents <blob>` streamed from the `cat-file` process
  straight into the response body with its `Content-Length`. A client
  that goes mid-stream takes the `cat-file` process with it (killed, not
  returned to the pool). `HEAD` answers the same headers and no body.
- **A listing** (5.5): `ls-tree -r -l -z --full-tree <commit>` joined
  with the commit's last-commit map, sorted by path bytes, cached per
  commit (immutable), paged with a cursor that names the commit and the
  offset. `commits` maps each page's last commits to `{author, date,
  message, order}`, `order` from one `rev-list --topo-order` walk per
  listed commit, cached.
- **History** (5.12): `log --date-order` with `--skip` and
  `--max-count`, NUL-separated fields.

## The last-commit index

The service defines a file's last commit as git's `log -1 -- <path>`
from the ref: the most recent commit that changed the path, following
the first TREESAME parent through merges (git's default history
simplification). macrofiche computes it per commit as it writes it:

- a root commit: every path's last commit is itself;
- otherwise, for each path at C: the first parent P whose entry for the
  path is the same (mode and blob) gives `last(P, path)`; if none, C.

`diff-tree` against each parent says which paths differ. A record holds
only the paths whose last commit differs from the first parent's, and
the paths removed; every 32nd record along the first parents (and every
root) is a full snapshot, so a map is rebuilt from at most 32 records.
Records are written like loose objects (a temporary file, fsync,
rename) before the ref moves. A commit with no record (written by a git
push, later) is computed the same way from its parents, recursively, in
one bounded walk; that is also the rebuild. Maps are cached in memory
(bounded).

## The jail

Each git process gets, from `macrofiche-git`'s `Jail`:

- **Landlock** (ABI ≥ 4; refused to start below 1 unless the operator
  sets `jail: "off"` for a non-Linux host): read and execute on the git
  prefix and `/usr`; read on `/etc/ld.so.cache` and `/dev/urandom`;
  read and write on `/dev/null`; every file right on its repository
  directory (whose `macrofiche/tmp` is its `TMPDIR`); nothing else.
  With ABI ≥ 4, TCP bind and connect are refused; with ABI ≥ 6, abstract
  unix sockets and signals outside its domain too.
- **seccomp**: `socket` with any family but `AF_UNIX` answers `EPERM`;
  `ptrace`, `process_vm_*`, `bpf`, `perf_event_open`, `keyctl`,
  `unshare`, `setns`, `mount` and their kin answer `EPERM`.
- **rlimits**: `RLIMIT_CPU`, `RLIMIT_DATA` (memory; file-backed pack
  maps are not counted, so a large read stays cheap), `RLIMIT_FSIZE`,
  `RLIMIT_NOFILE`, `RLIMIT_CORE` 0.
- `PR_SET_NO_NEW_PRIVS`; `PR_SET_PDEATHSIG` `SIGKILL`; its own process
  group.
- **A deadline** per operation, after which the group is killed; and a
  guard that kills the group when the request that owns the process is
  dropped (its client went away).
- **Environment**: cleared, then `GIT_DIR`, `GIT_EXEC_PATH`,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`,
  `GIT_ATTR_NOSYSTEM=1`, `GIT_NO_REPLACE_OBJECTS=1`,
  `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, `TZ=UTC`, `TMPDIR`, and `PATH`
  holding only the prefix's `bin`. No `HOME`.

A probe test runs jailed git against another repository, a file outside
the jail, and a TCP listener, and requires each to be refused.

## Limits

Every limit is a constant in `macrofiche-contract` (or the crate that
owns it) and is checked where input arrives.

| Limit | Value | Why |
|---|---|---|
| decoded bytes per `blob_chunk` | 4 MiB (413) | the service's |
| a pack line | 8 MiB (metadata: 16 MiB) | a 4 MiB chunk is 5.6 MB of base64 |
| files per pack | 20 000 | the metadata line's bound |
| bytes per blob | 1 GiB | four times fragment's largest pointer target |
| decoded bytes per pack | 2 GiB | |
| path | 4096 bytes | git's practical bound |
| repo name | 1–128 bytes | |
| branch name | 1–255 bytes | |
| commit message | 64 KiB | |
| JSON bodies (create, branch, merge, restore) | 1 MiB | |
| token | 8 KiB; lifetime at most 3600 s | the service's restricted keys' bound, applied to all |
| `files/metadata` `limit` | default 200, at most 1000 | the service's |
| `GET /api/repos` `limit` | default 20, at most 100 | the service's |
| `commits` `limit` | default 20, at most 100 | the service's |
| tree entries per listing | 200 000 | a cached listing's memory |
| commit packs in flight | 64 | spool space |
| git processes at once | 256 | |
| `cat-file` processes kept per repository | 4 | |
| request headers | 30 s | |
| a request body | 60 s idle; a deadline of 60 s plus 1 s per 16 KiB | half the CLI's 32 KiB/s floor, so the CLI's own deadline always ends first |
| git, per operation | reads 30 s; writes 60 s plus 1 s per 16 KiB; maintenance 10 min | |
| webhook attempts | 5, at 1, 2, 4, 8 s, 10 s each | |

## Recovery and maintenance

- **At boot, before serving** (macrofiche is the only writer, so no
  writer of its own can be alive): every `*.lock` under each repository
  (refs, `packed-refs`, `config`, `HEAD`, `shallow`, the commit-graph and
  multi-pack-index chains); temporary objects and packs (`tmp_obj_*`,
  `tmp_pack_*`, `tmp_idx_*`, `incoming-*` quarantines); the temporary
  refs `refs/macrofiche/tmp/*`; the index's temporary files; `spool/`
  and `tmp/`. What was removed is printed as JSON.
- **Idle-time maintenance**: a repository idle for 60 s with writes since
  its last maintenance gets `git maintenance run` with
  `geometric-repack`, `pack-refs`, `loose-objects` and `commit-graph`,
  under its write lock (so never beside a writer), with a deadline; once
  a day, `gc --cruft --prune=2.weeks.ago` as well. Index records of
  commits no ref reaches are dropped with them.

## Webhooks

After every branch move (commit pack, merge, restore, branch create,
and later a push), one delivery per moved branch to each of the org's
configured URLs, none when `before = after` or the ref is ephemeral. The
body is the union of the fake's and the service's:

```json
{"type": "push", "org": "<org>",
 "repository": {"repo_id": "…", "repo_name": "…", "url": "…", "id": "…"},
 "ref": "refs/heads/main", "before": "<40 hex>", "after": "<40 hex>",
 "pushed_at": "<RFC 3339>"}
```

Headers: `content-type: application/json`, `x-pierre-event: push`,
`x-pierre-signature: t=<unix s>,sha256=<hex HMAC-SHA256(secret, "<t>.<body>")>`,
`user-agent: Pierre-Webhook/1.0`. Each attempt is signed again. The
queue is in memory and bounded; delivery is at least once while the
process lives, best effort across a restart (the cell's poll catches a
lost one).

## Phases

Each phase ends with its tests green, `cargo clippy --workspace
--all-targets -- -D warnings` clean, its evidence below, and a commit.

1. **P1, the design**: this document, the contract's section 15, the
   README. *Exit:* the open questions answered, the crates and phases
   named.
2. **P2, the engine**: `cargo xtask engine` builds git 2.55.0 from its
   checked tarball; `macrofiche-git` wraps `fast-import`, `update-ref
   --stdin`, `merge-tree`, `commit-tree`, the `cat-file` pool, `ls-tree`,
   `log`, `for-each-ref`, `diff-tree`, `merge-base`; every process
   jailed. *Exit:* a test per plumbing call against the built engine, a
   conformance test per classified stderr message, the jail probe.
3. **P3, the service**: every route of the contract, auth, streaming
   packs and files, the CAS, merges and restore and the preview, the
   last-commit index, webhooks, boot recovery, idle maintenance.
   *Exit:* each route driven over HTTP by a test.
4. **P4, tests and evidence**: the integration suite against the real
   binary (valid, invalid, replay; 16 racing writers; kill -9 and
   restart with `fsck --strict`; a 100 MiB blob in and out with RSS
   measured; merges and restore; the webhook's signature and retries;
   the fake's own tests where they describe the contract), and
   `docs/evidence/*.json`. *Exit:* the evidence below.
5. **P5, smart HTTP** (if time remains): `git clone`, `fetch` and `push`
   with the same JWT, through `upload-pack` and `receive-pack
   --stateless-rpc`, keeping the CAS, the index and the webhook. *Exit:*
   the real git client clones, pushes, and is refused a stale push.

## Evidence

Measured on the library spike's machine (AMD Ryzen 9 9950X3D, 32
threads, Linux 7.2.5 with Landlock ABI 10, btrfs on NVMe, rustc 1.99.0).

### P2, the engine (`docs/evidence/engine.json`)

- **The build.** `cargo xtask engine` downloads git 2.55.0 (12.2 MB),
  checks its SHA-256 (`0842dc38…3d1e`), and builds it into
  `target/engine/git-2.55.0` in 5.9 s on 32 cores (87 MB installed; it
  links only libc and libz). git 2.55's own Rust part is built, as its
  default build does.
- **Tests:** 20 unit (parsers, the jail's plan, the seccomp program run
  by an interpreter, the fast-import stream, the pin), 9 conformance
  (each update-ref refusal, unknown objects in five calls, unrelated
  histories, fast-import's refusals, each produced by the engine), and 6
  engine tests (every plumbing call, the pool, a large blob, the jail,
  deadlines).
- **The jail holds.** A jailed git pointed at another repository answers
  "not a git repository"; at a file outside, "Permission denied"; asked
  to create a repository outside, "cannot mkdir … Permission denied";
  sent to a TCP listener on 127.0.0.1, "errno=Operation not permitted",
  and the listener sees no connection. With the jail off, the same four
  succeed and the listener is reached (the control).
- **A blob streams.** 48 MiB of incompressible bytes go in from a spool
  through `fast-import` and out through a pooled `cat-file`, checked
  byte for byte; the reader's heap peaks at 308 KiB.
- **Found by the conformance tests:** `rev-list` says "bad object" for an
  unknown id, which the first reading missed (it would have been a 500,
  not a 404).

## Debt

*This list is macrofiche's ledger: each item stays here until it is paid.*

## Risks

- **A process per call.** About 1.4 ms and a fork per git call; the
  `cat-file` pools take most reads off that path. Measured in phase 4,
  on one machine.
- **Text interfaces.** Errors are classified from stderr; the pinned
  version and a conformance test per message keep a 409 a 409.
- **fsync on the node's filesystem.** Durable write latency is the
  disk's; the numbers here are btrfs on NVMe.
- **Landlock's reach.** An older kernel (ABI < 4) cannot refuse TCP by
  Landlock; the seccomp filter still does.
