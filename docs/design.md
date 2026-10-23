# macrofiche: the design

*2026-10-03.* Paul: "Make code.storage into a real self-hosted git store,
make it another sibling project like sandcastle… follow the same
engineering-style as sandcastle… base it on [a] low level library like
libkrun… support all the features we use of code.storage including good
blob support."

What macrofiche answers is docs/contract.md (fourteen routes and a
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
  restore refuses with 412, a commit that would change nothing is
  refused with 412 (`no changes to commit`), history is in commit-date
  order.

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
| `crates/server` | `macrofiche`: the binary. Config, the HTTP server and every route, streamed bodies, webhook delivery, smart HTTP | config, webhook signing, smart-HTTP paths pure |
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
    macrofiche/repo.json           {repo_id, repo_name, org, default_branch, created_at, deleted_at_ms?}
    macrofiche/lastcommit/aa/<38>  the last-commit index, one record per commit (like loose objects)
    macrofiche/tmp/                git's TMPDIR, inside the jail
  spool/<request>/<content_id>     a commit pack's decoded blobs while it uploads
  tmp/                             repositories being created
```

A repository's directory is named by its `repo_id` (random, 32 hex),
never by its name, so a name may be anything the contract allows (the
service's names may hold `/`). The registry, name to id and id to name,
is read from every `repo.json` at boot and kept in memory: macrofiche is
the only writer. A deleted repository (5.13) keeps its `repo.json`,
marked with `deleted_at_ms`, and nothing else: it serves nothing, leaves
the org's list, and its name and id stay taken.

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
| Write order | spool, then lock, then `fast-import`, then the no-change check (412), then the index record (fsynced), then `update-ref` (CAS), then answer | the client's upload never holds the lock; an acknowledged commit always has its record |
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
2. **Webhooks: per org in the config, and per repository by
   macrofiche's own route.** The org's hooks (a URL and a secret file
   each) are the service's model: one org-wide subscription, made in its
   dashboard. But the cell verifies each delivery with its fragment's own
   secret, routed per fragment, so an org-wide hook cannot reach a
   fragment. macrofiche therefore adds an extension, not the service's:
   `PUT /api/repos/{repo}/webhook {url, secret}` (scope `repo:write`; at
   most 4 per repository, replaced by URL) and `DELETE
   /api/repos/{repo}/webhook?url=`. The cell can call it at create and at
   `rotate`, and the e2e as it calls the fake's `register_webhook`. A
   move is delivered to both kinds. Fragment uses no webhook in
   production today (its pins move by `refresh`, its own commits, and the
   poll), so nothing changes until it registers. *Paul to confirm* the
   cell calls the route.
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
   linear history it equals first parents. `limit` defaults to 20, at
   most 100, with an opaque cursor that pins the tip it started from.
   Since PR #127, `live` holds merge commits after a deploy that follows
   a rollback, so its history in date order includes main's commits
   merged in, sorted by date among the deploys; the fake walks first
   parents. PR #127's own test reads `live`'s history by first parents
   (the port here asserts what holds either way), and `rollback`'s
   default target (element 1) can be a main commit on the service. *Paul
   to confirm*: fragment may want `drafts` and `rollback` to walk first
   parents themselves.
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
4. If the new commit's tree is its parent's (on a branch it would
   create, the empty tree), the pack changes nothing: the temporary ref
   is deleted, and the answer is the service's 412, `no changes to
   commit` (contract 5.8). No record is written, nothing moves, nothing
   is announced; the unreferenced commit is maintenance's to prune.
5. `diff-tree` between the tip and the new commit gives the changed
   paths; the last-commit record is written and fsynced.
6. One `update-ref --stdin` transaction: `update refs/heads/<b> <new>
   <tip>` (or `create`), `delete` of the temporary ref. git's ref
   transaction is the compare-and-swap; it fsyncs the ref.
7. Answer 201; queue the webhook; remove the spools.

A pack whose answer is lost has landed by step 6, so a client that
re-reads sees it (contract section 9), and a client that sends it again
gets step 4's 412.

### Merge (5.10), preview (15.1), restore (5.11), branch create (5.9)

- **Merge**, under the write lock: resolve source and target (404),
  check `expected_target_sha` (409 `target_moved`), then decide with
  `merge-base`: `no_op` (the target holds the source), fast-forward
  (unless `strategy: merge`, which always makes a merge commit; refused
  for `ff_only` when diverged), or `merge-tree --write-tree` (409
  `merge_conflict` with the paths), then `commit-tree` with parents
  `[target, source]` (or `[target]` for `squash`), the index record,
  and the CAS. A squash whose tree is the target's is the commit pack's
  412 (`no changes to commit`); a merge commit is made even then (git's:
  its second parent records the merge). Unrelated histories are 400
  unless `allow_unrelated_histories`.
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
- **Repo delete** (5.13), under the write lock: `repo.json` marked
  `deleted_at_ms` (written durably, as every `repo.json` is), the
  handle marked gone (a write that waited on the lock is refused, 404),
  the registry marked, then every other file in the directory removed.
  A delete again is 409 `repository_deleted`, and so is a create of the
  name.

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
  and `tmp/`; and whatever a deleted repository still holds beside its
  `repo.json` (a delete cut short after its mark). What was removed is
  printed as JSON.
- **Idle-time maintenance**: a repository idle for 60 s with writes since
  its last maintenance gets `git maintenance run` with
  `geometric-repack`, `pack-refs`, `loose-objects` and `commit-graph`,
  under its write lock (so never beside a writer), with a deadline; once
  a day, `gc --cruft --prune=2.weeks.ago` as well. Index records of
  commits no ref reaches are dropped with them.

## Webhooks

After every branch move (commit pack, merge, restore, branch create,
and later a push), one delivery per moved branch to each of the org's
configured URLs and each URL the repository registered (`PUT
/api/repos/{repo}/webhook`, question 2), none when `before = after` or
the ref is ephemeral. The body is the union of the fake's and the
service's:

```json
{"type": "push", "org": "<org>",
 "repository": {"repo_id": "…", "repo_name": "…", "url": "…", "id": "…"},
 "ref": "refs/heads/main", "before": "<40 hex>", "after": "<40 hex>",
 "pushed_at": "<RFC 3339>"}
```

Headers: `content-type: application/json`, `x-pierre-event: push`,
`x-pierre-signature: t=<unix s>,sha256=<hex HMAC-SHA256(secret, "<t>.<body>")>`,
`user-agent: Pierre-Webhook/1.0`. Each attempt is signed again; five
attempts at most, 1, 2, 4 and 8 s apart, 10 s each, until a 2xx. The
queue is in memory and bounded (10 000 waiting, 64 delivering);
delivery is at least once while the process lives, best effort across a
restart (the cell's poll catches a lost one). A repository's registered
secrets live in its `macrofiche/webhooks.json`, mode 0600.

## Smart HTTP

`git clone`, `fetch` and `push` at `<base>/<repo>.git`, as the service
serves them at `https://t:<JWT>@<org>.code.storage/<repo>.git`
(contract section 11). macrofiche serves git's own protocol as git's
`http-backend` does: `GET <repo>.git/info/refs?service=…` runs
`upload-pack` or `receive-pack --stateless-rpc --http-backend-info-refs`
(the `# service=` header but under protocol v2), and `POST
<repo>.git/git-upload-pack` and `git-receive-pack` run the same with
`--stateless-rpc`, each jailed like every git. `Git-Protocol` is passed
through as `GIT_PROTOCOL`, so fetches speak v2; a gzipped request (git's
client gzips its small ones) is read whole, at most 64 MiB, and
inflated, at most 256 MiB.

- **Auth** is the REST API's: the JWT as the Basic password (any user
  name) or as a bearer; `git:read` to fetch, `git:write` to push; the
  `repo` claim must name the path's repository (else 404). Without
  credentials the answer is 401 with `WWW-Authenticate: Basic`, so git
  sends the ones its URL holds. Dumb HTTP is refused.
- **A push is one more write** (library.md, Risks: two write paths). Its
  body is spooled first (at most 2 GiB, `receive.maxInputSize` too), so
  a slow client holds nothing; then `receive-pack` runs under the
  repository's write lock, so a push and a commit pack never interleave.
  receive-pack's own ref transaction is the compare-and-swap of each
  command's old value (a stale one is `ng … failed to update ref`). The
  refs it moved, created or deleted under `refs/heads/` get their
  last-commit records before the answer, and their webhooks after it.
  `transfer.hideRefs` hides `refs/macrofiche/` and the ephemeral
  namespace from both directions, so a push cannot write them; objects
  are checked (`receive.fsckObjects`) in git's quarantine; no hook runs
  (an empty `core.hooksPath`, and Landlock grants no execute in the
  repository).
- **A clone** streams `upload-pack`'s pack to the client as it is made;
  a client gone ends the process group.

## Config and command line

`macrofiche serve --config <file>` reads one JSON file, checked at start
(unknown fields refused). Only `listen`, `state_dir` and `orgs` are
required, which is exactly what fragment's dev stack writes
(`crates/devstack/src/codestore.rs` on its `selfhost-macrofiche` branch):

```json
{"listen": "127.0.0.1:9600",
 "state_dir": "/var/lib/macrofiche",
 "orgs": [{"name": "fragment-dev",
           "keys": [{"pem_file": "/etc/macrofiche/fragment-dev.pub.pem", "kid": "k1"}],
           "webhooks": [{"url": "https://…", "secret_file": "/etc/macrofiche/hook.secret"}]}],
 "git_prefix": "/opt/macrofiche/git-2.55.0",
 "jail": "enforce",
 "tls": {"cert_file": "/etc/macrofiche/tls.crt", "key_file": "/etc/macrofiche/tls.key"},
 "maintenance_idle_s": 60}
```

- `keys`: each `pem_file` or `pem` (inline SPKI PEM; a public key is no
  secret), and an optional `kid`.
- `webhooks`: optional; secrets only by file.
- `git_prefix`: optional; else `MACROFICHE_GIT_PREFIX`, else beside the
  binary as `cargo xtask engine` builds it (`target/release/macrofiche`
  finds `target/engine/git-2.55.0`).
- `jail`: `enforce` (the default) or `off`, the operator's choice for a
  host without Landlock.
- `tls`: optional; without it macrofiche serves plain HTTP for a private
  network or a proxy in front.

Once it answers, it prints one line to stdout, `{"event":"serving",
"addr":"…","tls":…,"landlock_abi":…,"pid":…}`; its log is JSON lines on
stderr (recovery, maintenance, webhook failures, store errors). SIGTERM
or SIGINT stops it accepting and gives requests in flight 15 s.
`macrofiche check --config <file>` reads a config, its keys and its
secrets, and prints what it would serve; `macrofiche pubkey <pkcs8.pem>`
prints the SPKI PEM of fragment's `CODESTORAGE_PRIVATE_KEY` for a
config's `keys`; `macrofiche version` names the pinned git.

## Dependencies

Pinned exactly in the workspace's `Cargo.toml`; each vetted when it came
in. All but `base64` are sandcastle's own pins or in its tree.

| Crate | Version | Why | Vetted |
|---|---|---|---|
| hyper, hyper-util, http-body-util | 1.11.0, 0.1.20, 0.1.5 | HTTP/1.1 server and webhook client | sandcastle's pins |
| tokio | 1.53.1 | runtime, processes, timers | sandcastle's pin |
| rustls, tokio-rustls, webpki-roots | 0.23.43, 0.26.4, 1.0.9 | optional TLS listener, https webhooks, the engine's download | sandcastle's pins, ring provider |
| ring | 0.17.14 | ECDSA P-256 verify, HMAC-SHA256, SHA-256, random ids | in sandcastle's tree (rustls); one crypto library instead of p256, hmac, sha2 |
| base64 | 0.22.1 | blob chunks, JWT segments | no dependencies; MIT/Apache-2.0; the de facto crate |
| serde, serde_json, thiserror | 1.0.229, 1.0.151, 2.0.20 | wire types, typed errors | sandcastle's pins |
| libc | 0.2.189 | Landlock, seccomp, prctl, rlimits, renameat2 | sandcastle's pin |
| flate2, tar | 1.1.10, 0.4.46 | the engine's tarball (xtask only) | sandcastle's pins |

The `landlock` crate was considered and not taken: it pulls `anyhow`,
`enumflags2`, `lazy_static` and `strum`, and three raw syscalls (as
sandcastle does its seccomp) are less to vet.

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
5. **P5, smart HTTP**: `git clone`, `fetch` and `push` with the same
   JWT, through `upload-pack` and `receive-pack --stateless-rpc`, keeping
   the CAS, the index and the webhook. *Exit:* the real git client
   clones, pushes, and is refused a stale push.

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

### P3 and P4, the service and its evidence

The release binary, driven over loopback by `macrofiche-bench` (one
fresh TCP connection per request), each scenario on its own state
directory; `cargo run --release -p macrofiche-bench -- all --write`
writes `docs/evidence/{latency,blob,race,crash,maintenance}.json`.

**Tests.** 91 in all: the pure crates' (contract 9, auth 4, pack 5, store
7, server 4, git 20), the engine's against git 2.55.0 (conformance 9,
engine 6), the store's against the engine (5, every listed file's last commit checked against git's own `log -1`
through fast-forwards, a rollback, three-way merges, a conflict, and
more than one snapshot interval), and 22 integration tests against the
real binary: every route valid, invalid and replayed (7), fragment's own
tests where they describe the contract (9: the fake's JWT test with the
service's statuses, six of the CLI's, PR #127's deploy after a rollback
with its `Promotion` logic ported, a lost answer), 16 racing writers,
kill -9 rounds, a 100 MiB blob, and the webhook (2).

**fragment's e2e** (`docs/evidence/fragment-e2e.json`), run by fragment's
own harness on its `selfhost-macrofiche` branch against the release
binary started with the config its dev stack writes: `codestore`,
`create`, `deploy`, `files` and `sync`, 123 passed, 0 failed, 6 skipped
(each for the fake's levers, the browser, or a webhook nobody
registered). Its 22 `codestore` probes of the contract all pass,
including the five that hold only on a store that follows the service
(the url form, problem bodies, ephemeral refs, restore to the tip, a
token for another repo).

**Latency** (`latency.json`, 200 samples a route, a repository of 106
files; ms):

| Route | p50 | p95 | max |
|---|---|---|---|
| `POST commit-pack`, 1 file, durable | 7.27 | 7.83 | 11.26 |
| `POST commit-pack`, 5 files, durable | 8.68 | 9.32 | 9.73 |
| `GET branch` | 0.13 | 0.17 | 0.24 |
| `GET file`, 1 KiB | 0.14 | 0.17 | 0.58 |
| `HEAD file` | 0.10 | 0.13 | 0.20 |
| `GET files/metadata`, a new head (ls-tree, the map, the topo walk) | 3.48 | 4.48 | 4.67 |
| `GET files/metadata`, the same head again (cached) | 0.41 | 0.50 | 0.63 |
| `GET commits`, 30 | 2.10 | 2.96 | 3.14 |
| `GET merge/preview` (merge-base, merge-tree) | 9.72 | 13.94 | 14.59 |
| `POST merge`, three-way, durable | 13.42 | 17.88 | 18.49 |
| `POST restore-commit`, durable | 7.92 | 10.28 | 11.27 |
| `POST branches/create` | 2.18 | 2.41 | 2.65 |
| `POST /api/repos` (50) | 3.64 | 4.31 | 6.93 |
| `GET /api/repos`, 100 (50) | 0.17 | 0.22 | 0.22 |

A durable commit is three git processes (fast-import, diff-tree,
update-ref) and two fsynced files of macrofiche's (the index record and
nothing else on a commit to an existing branch): 7.3 ms against the
spike's 8.9 for fast-import and update-ref alone. Reads through the
pooled `cat-file` stay near 0.1 ms. After the 3,000-odd requests,
macrofiche held 10 MiB of heap (16 MiB resident).

**A 100 MiB blob** (`blob.json`): 139.8 MB of commit pack in, 3.7 s
(27 MiB/s, client encoding included), and out in 47 ms (2.1 GiB/s from
the page cache), checked byte for byte with git's blob id. Peak heap:
macrofiche 50.6 MiB in and 29.3 MiB out, fast-import 29.3 MiB, the
`cat-file` reader 0.3 MiB. Neither grows with the blob: macrofiche holds
one line and one decoded chunk at a time, git streams past
`core.bigFileThreshold`.

**Sixteen writers on one branch** (`race.json`), each until 20 of its
commits land, retrying on 409 as the CLI does: 320 landed in 2.5 s (127
commits/s) out of 2,921 attempts (2,601 refused with 409, nothing
applied), 321 commits of linear history and 321 files; a pack's answer
p50 7.7 ms, p99 8.6. Sixteen writers on repositories of their own: 784
commits/s.

**kill -9** (`crash.json`): 20 rounds of four writers committing 256 KiB
files, the server killed after 100 to 700 ms with 3 to 5 git processes
running, restarted on the same state: `git fsck --full --strict` clean
after every round, 982 acknowledged commits and none lost, the next
write landing every time; a restart serves in 4 to 6 ms. Boot recovery
cleared, across the rounds, 1 stale ref lock, 70 spools, 11 temporary
objects, and 4 temporary refs. Every git died with the server
(`PR_SET_PDEATHSIG`).

**Maintenance** (`maintenance.json`): after 300 commits, 1,200 loose
objects (4.8 MB) became one pack of 110 KiB in 300 ms (geometric repack,
loose objects, pack-refs, commit-graph, and the day's gc), under the
repository's write lock; `GET commits` went from 2.67 to 1.42 ms.

### P5, smart HTTP (`docs/evidence/smart.json`)

The real git client (the system's git 2.55.0, built with curl; the
pinned server-side build has none) against the release binary, in
`crates/server/tests/smart.rs` (3 tests) and the driver:

- it clones what the REST API wrote (protocol v2 and v0), pushes a
  commit back, and the REST API reads the push: the branch's head, the
  new file, and git's last commits (the pushed commit for what it
  changed, the old commit for the rest); the push's webhook names its
  move; a REST commit after it is fetched; a branch made and deleted by
  push is announced both times;
- it is refused a push on a stale lease, a non-fast-forward without
  force, and a push to `refs/macrofiche/`; and by hand (the protocol
  written out, since the client checks its lease before it sends), a
  command whose old value is stale is `ng` with nothing moved, and the
  same command from the right old value lands;
- auth: no credentials is 401 with a Basic challenge, a read token
  clones and cannot push (403), another repository's token finds nothing
  (404), dumb HTTP is 403;
- a clone of 1,000 files and 100 commits takes 30 ms (p50 of 5), a fetch
  of nothing 7 ms, a push of one commit 14.7 ms (p50 of 50, p95 15.9),
  each a whole run of the client.

With P5 the workspace has 96 tests (two more pure, three against the
real client).

### After P5: no empty commits (2026-10-06)

fragment PR #163 found that the service refuses a commit pack that
changes nothing (412, `no changes to commit`), and its cell now answers
main's tip for that refusal; macrofiche made an empty commit instead
(fragment's `docs/self-host.md`, found 33). A commit pack or a squash
that changes nothing is now that 412, the service's body exactly
(contract 5.8). Three tests more, 99 in all: the store's (each kind of
such pack, the compare-and-swap first, a mode as a change, a squash
refused while a merge commit is made, no temporary ref left, a reopen),
one over HTTP (the body, no webhook, the replay after a kill -9 and a
restart), and the fake's own, ported. fragment's `codestore` and
`templates` sections on celld against the release binary: 64 passed, 0
failed; against main's, 62 and 2 (templates' "a write of what main
holds commits nothing" and its "main moves" check).

## Debt

*This list is macrofiche's ledger: each item stays here until it is paid.*

- Smart HTTP serves no Git LFS batch API (question 5: not worth it
  until computers' agents push large files with `git lfs`), no
  `git-upload-archive`, and `receive-pack` speaks protocol v0 (git's
  own does not do v2 for pushes).
- receive-pack's answer (its report and progress) is held whole, at most
  16 MiB, so its records and webhooks can follow it; a push of many refs
  answers only when every record is written.
- A push deleting a branch is announced with `after` all zeros; fragment
  never deletes a branch, and the cell reads such a delivery as a move to
  nothing.

- `GET file` honors `If-None-Match` (304) but not `Range`, `If-Range`,
  `If-Match` or the date conditionals the service documents, and sends
  no `accept-ranges`; fragment sends none of them.
- The webhook queue is in memory: deliveries waiting at a restart are
  lost (the cell's poll and `refresh` catch them).
- The index is rebuilt (a commit macrofiche did not write, or a lost
  index) with a `diff-tree` per commit: about 1.5 ms each, written as it
  goes. One `git log --raw` stream would be faster, but cannot say which
  parent an empty per-parent diff belongs to.
- Index records of commits no ref reaches (a CAS that lost, an abandoned
  merge) are never removed; they are small.
- `FileLastCommit.order` takes one `rev-list --topo-order` walk per newly
  listed commit, as deep as its oldest last commit.
- A path that is not UTF-8 (only a future push could write one) is read
  lossily.
- A branch macrofiche did not see made (none today) reports its tip's
  committer date as `created_at`.
- The merge preview's `include_content` renders no conflict content; each
  conflict is listed in `filtered_conflicts`.
- `force` on `branches/create`, and `base_repo` and `upstream` on create,
  are refused with 400.
- No rate limiting (429); overload answers 503.
- Maintenance holds the repository's write lock; a large repack would
  hold its writers up to the 10-minute deadline.
- Ephemeral refs are never expired; fragment never deletes a preview.
- Spools are bounded per pack (2 GiB) and by 64 packs in flight, not in
  total bytes.
- After a large upload macrofiche's resident memory stays at its
  high-water mark (the allocator keeps freed arenas).
- A repository's webhook secrets sit in its directory (mode 0600), not
  in a secret store.
- `GET /api/repos` sorts the org's repositories in memory for each page.
- The `merge` strategy is read as "always a merge commit" and `ff_only`'s
  refusal as 409 `not_fast_forward`; the service's documents leave both
  open.
- The TLS listener is built and smoke-tested by hand, not by the suite.
- A deleted repository's `repo.json` stays for good (its name is never
  made again), and counts toward its org's 100,000 repositories.

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
