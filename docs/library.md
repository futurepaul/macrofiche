# The git library under macrofiche

*2026-10-03. Paul: "Try to find a similarly low level library like
libkrun to base it on." What macrofiche must answer is docs/contract.md;
this picks what answers it.*

## Summary

**Build macrofiche on git itself: a pinned git binary, driven through its
plumbing, one process per operation, jailed, the way GitLab's Gitaly
drives git.** Not on libgit2 and not on gitoxide. In sandcastle's terms,
git is the libkrun here: the engine, built at a pinned version into
macrofiche's prefix, run by path inside a jail of macrofiche's making,
with the API and the policy (tokens, compare-and-swap, webhooks, limits)
macrofiche's own.

Why, briefly (the spike's numbers are medians of 5 on one machine; the
evidence is in `spikes/library/evidence/`):

1. **Only git has every part.** The later smart-HTTP half (upload-pack
   and receive-pack, protocol v2) exists in no Rust-reachable library:
   libgit2's transports are client-only, and gitoxide's maintainer
   declined both server-side pull requests in August 2026 and asked that
   they live in forks. Neither library has gc: libgit2 has a pack builder
   and nothing that prunes or repacks, and gitoxide's pack writer does no
   delta compression at all.
2. **Durability.** The contract says objects are synced before the ref
   moves and a restart loses nothing acknowledged (contract, section 13).
   gitoxide never fsyncs: not one `fsync`, `sync_all`, or `sync_data`
   in the 49 `gix-*` crates of this build. libgit2 does, when asked
   (`GIT_OPT_ENABLE_FSYNC_GITDIR`, off by default). git does with
   `core.fsync=all`.
3. **The reference semantics.** All three wrote byte-identical objects in
   the spike (the same commit, tree, blob, and merge ids, every sample,
   all checked by `git fsck --full --strict`). For merges git's
   merge-ort is the reference: GitHub moved its server-side merges from
   libgit2 to it (p50 71 ms to 7.7 ms, and an end to merges rejected that
   succeeded locally), GitLab has an epic to drop libgit2 from Gitaly,
   and gitoxide's author says its tree merge needs a rewrite "so that the
   whole logic can be proven to be correct".
4. **The price is a process per call, and it is small next to fsync.** A
   durable commit of five files took 8.9 ms through git (fast-import and
   an `update-ref` transaction) and 9.4 ms through libgit2 writing loose
   objects, run side by side; under IO pressure, 12 ms against 79.
   Without fsync, git took 3.6 ms against 0.75 to 0.94 ms in-process. A
   read costs 1.4 ms in a fresh git process against about 0.1 ms
   in-process, and 3.4 to 4.5 ms for a hundred files through one
   long-lived `cat-file --batch`. fragment's clients allow 30 s a call
   (contract, section 1).

If an in-process library is required anyway, it is libgit2 (with the
pack-per-commit write path below), because it meets the durability
requirement today and gitoxide does not; and git is still needed for gc
and for smart HTTP. gitoxide is the better library to *read* with (the
fastest here, in Rust, no durability role on a read) and is the
candidate for an in-process read path later, once reads are measured hot.

## What macrofiche needs (the requirements)

From docs/contract.md and Paul's brief. "Now" is code.storage's REST
contract as fragment uses it; "later" is real git clients.

| # | Requirement | Source |
|---|---|---|
| R1 | Many bare repositories on local disk, created on demand | contract 5.1 |
| R2 | A file's bytes at a ref, with its blob id and its last commit; the tree at a commit, with sizes and last commits, paginated | 5.5 to 5.7, section 7 |
| R3 | A commit from adds, updates, and deletes (content streamed in, up to tens of MB a pack) on a branch, compare-and-swap on its head (409 when it moved) | 5.8, sections 8 and 9 |
| R4 | Branch creation (CAS: one creator wins); fast-forward and true merges against an expected target, conflicts detected; restore an earlier tree as a new commit | 5.9 to 5.11 |
| R5 | History, first parents, paginated | 5.12 |
| R6 | Later: smart HTTP for `git clone` and `git push` (upload-pack and receive-pack, protocol v2), under the same CAS and webhook rules | section 13 |
| R7 | Concurrency: many writers to one repo, many repos at once, reads without locks | section 9 |
| R8 | Crash safety: objects durable before the ref moves, refs atomic, nothing acknowledged lost, nothing torn after kill -9 or power loss | section 13 |
| R9 | gc and repacking: packs with deltas, garbage collected | section 13 |
| R10 | Large blobs streamed in and out, never whole in memory where avoidable. Today files of 1 MiB or more are LFS pointers, so blobs in git are small (section 8); open question 14.5 may change that | section 8 |
| R11 | A build and a license that let us ship binaries (sandcastle had libkrunfw's GPL question) | brief |
| R12 | Maintained, with a future: SHA-1 now (section 4), SHA-256 and reftable as git 3 arrives | brief |

## The candidates

### libgit2, through git2

- **What.** The C reimplementation of git as a library (since 2008), and
  its Rust bindings. Pinned for the spike: `git2 =0.21.0` (2026-05-18)
  over `libgit2-sys =0.18.8+1.9.7` (2026-08-21), which builds libgit2
  v1.9.7 (2026-08-13) from vendored source.
- **Maintenance.** Releases continue on two lines (1.8.x, 1.9.x), mostly
  security and bug fixes; no 2.0. In the twelve months to 2026-10-03,
  371 commits by 42 authors, 252 of them by its maintainer, Edward
  Thomson. git2-rs: 396 commits by 14 authors, under the rust-lang
  organisation.
- **License.** GPLv2 with the linking exception: a program that links
  libgit2 may be distributed under any terms; libgit2's own files stay
  GPLv2 (changes to them are shared). Two notes for shipping binaries:
  the vendored build also compiles `deps/xdiff`, which is LGPL-2.1+
  (LibXDiff), statically, and the exception in libgit2's COPYING does
  not speak for it; and git2-rs itself is MIT or Apache-2.0. cargo
  itself depends on git2 (and on gix), so the shape is common, but it is
  the libkrunfw kind of question.
- **Build.** `cc` compiles about 118,000 lines of libgit2 C and 61,000
  lines of bundled dependencies (xdiff, llhttp, pcre2, sha1dc); no cmake.
  zlib links dynamically from the system unless `libz-sys` is told to
  build statically. A musl static build needs a musl C toolchain. The
  Rust side is 6 crates.
- **Against the requirements.**
  - R2, R3, R5: everything is there. `TreeUpdateBuilder`
    (`git_tree_create_updated`) applies nested upserts and removals to
    a base tree in one call; `reference_matching` is a ref CAS.
  - R4: `merge_commits` and `merge_trees`, in memory, with an index of
    conflicts. Its merge-base choice differs from git's: one of the two
    reasons GitHub left it (the other was speed).
  - R6: none. `src/libgit2/transports/smart_protocol.c` negotiates a
    fetch and a push as a client; there is no server half.
  - R7: "libgit2 objects cannot be safely accessed by multiple threads
    simultaneously" (its docs/threading.md): one `Repository` per
    thread; `git_odb` locks internally. A held ref lock fails at once,
    with no retry (GIT_ELOCKED).
  - R8: `GIT_OPT_ENABLE_FSYNC_GITDIR` (off by default) fsyncs loose
    objects, refs, packs, indexes, the commit-graph, and the
    multi-pack-index (`git_repository__fsync_gitdir`'s uses).
  - R9: `git_packbuilder` builds packs with delta search; the indexer
    indexes them. No gc, no prune, no geometric repacking: those are
    ours.
  - R10: `git_blob_create_from_stream` spools an unknown-size stream to
    `objects/streamed` and streams it into a loose object.
    `git_odb_open_rstream` streams only loose objects: of the backends,
    only `odb_loose.c` has a `readstream` (`odb_pack.c` has none), so a
    packed blob is read whole. With an in-memory backend in front
    (`git2-packed` below), even a write stream is buffered whole
    (`init_fake_wstream`).
  - R12: SHA-256 is behind an "unstable" feature; no reftable.
- **Security.** In-process C parsing objects that users wrote. Recent:
  CVE-2024-24577 (heap corruption in `git_index_add`, fixed in 1.7.2)
  and CVE-2026-5917 (command injection through the libssh2 transport,
  fixed in 1.9.7; not built here).

### gitoxide, through gix

- **What.** git reimplemented in Rust, as about fifty `gix-*` crates.
  Pinned for the spike: `gix =0.88.0` (2026-09-25) and `gix-pack
  =0.75.0` (for its pack writer, behind features `gix` leaves off).
- **Maintenance.** The most active of the three: at least 1,500
  commits in the twelve months (the API's pages ran out there), from 46
  authors; of those 1,500, 731 are its author Sebastian Thiel's (as
  himself and as "Byron"), 345 are "Codex" and 109 "GPT 5.5". A breaking
  release every month (0.78 in January to 0.88 in September 2026); in
  September every error type changed (`thiserror` to `gix-error`).
- **License.** MIT or Apache-2.0, as are nearly all of the 112 crates
  `gix` pulls in here (a few Zlib, Unlicense, one MPL-2.0: `uluru`).
  Pure Rust: a static musl binary needs nothing else.
- **Against the requirements.**
  - R2, R3, R5: everything is there. `Repository::edit_tree` returns
    an editor for nested upserts and removals; ref transactions take
    `PreviousValue::MustExistAndMatch` (CAS) and wait on a held lock
    with backoff.
  - R4: `merge_commits` and `merge_trees`, in memory; "tree-diff
    heuristics match Git for its test-cases", and also "rewrite so that
    the whole logic can be proven to be correct - it's too insane now"
    (crate-status.md, gix-merge).
  - R6: none, and none coming upstream. crate-status.md: "upload-pack /
    receive-pack server plumbing for in-process transports" unchecked;
    `gix-transport` "general purpose `accept(…)` for servers" unchecked.
    Two pull requests added a server-side upload-pack (#2465, #2887);
    both were closed unmerged on 2026-08-06: "I can't merge huge and
    impactful features like these especially while I have no use for
    them", with an invitation to keep them in a fork.
  - R7: `ThreadSafeRepository` hands out a thread-local handle per
    thread; the object store refreshes itself when packs change on disk.
  - R8: **no fsync at all.** Writes are a temporary file and a rename,
    so a killed process tears nothing, but a power loss can leave a ref
    or an object empty. Closing that needs a sync before the rename,
    inside `gix-tempfile` and `gix-lock`: a change upstream or a fork.
  - R9: `gix-pack` writes packs, but "delta compression" is unchecked;
    `PackCopyAndBaseObjects` is its only mode (it copies deltas already
    in packs, and deflates everything else whole). No gc.
  - R10: no streaming read at all (a blob is decoded into one buffer,
    and the spike saw twice the blob's size in heap). For writes,
    `Repository::write_blob_stream` copies the whole stream into memory
    first; the object store's `write_stream` streams but needs the size
    up front, so an HTTP body is spooled first.
  - R12: SHA-256 partly (a `sha256` feature exists); reftable not
    started (the September 2026 report says git 3 is "maybe 4 - 6
    months" away).

### git itself, driven as processes

- **What.** The reference implementation, version 2.55.0 on this
  machine. Its plumbing is a low-level API with a process boundary:
  `fast-import`, `hash-object`, `mktree`, `commit-tree`, `update-ref
  --stdin` (transactions with old values: CAS), `merge-tree
  --write-tree` (merge-ort, since 2.38: a merge with no worktree and no
  index, exit 1 on conflicts and the conflicted paths listed),
  `cat-file --batch` and `--batch-command` (streaming reads through one
  long-lived process), `ls-tree`, `log`, `rev-list`, `diff-tree`,
  `upload-pack` and `receive-pack` (`--stateless-rpc`, protocol v2),
  `http-backend`, `maintenance`, `repack --geometric`, `gc`, `fsck`.
- **Who does this.** GitLab's Gitaly runs git as processes and keeps
  long-lived `cat-file` processes per repository; its epic "Deprecate
  Libgit2 in Gitaly" exists because "features in mainline git move
  faster than on libgit2". GitHub moved its server-side merges and
  rebases from libgit2 to merge-ort and `git replay`.
- **Against the requirements.** All of R1 to R10. R8 needs config:
  git's default hardens only packs (`core.fsync=committed,-loose-object`
  in effect), so macrofiche sets `core.fsync=all`. R10 needs config too:
  git streams a packed blob only above `core.bigFileThreshold` (512 MiB
  by default), so at the default `cat-file` held 50.3 MiB of heap for
  the packed 50 MiB blob; at `1m`, 0.3 MiB.
- **Costs.** A process per call (1.3 to 1.8 ms here); answers parsed from
  text and errors classified from exit statuses and stderr; a GPLv2
  program shipped beside macrofiche (aggregation, not linking: its
  source offered with it, as sandcastle ships libkrunfw); config
  hygiene (no system or global config, no hooks but ours).

### Others, briefly

- **go-git** (Go): v6 (alpha.5, 2026-07-29) has a server backend for
  upload-pack and receive-pack with protocol v2, but no merges beyond
  fast-forward. A Go service, not a library for a Rust one.
- **JGit** (JVM): the only library with a complete server half (Gerrit
  runs on it). Out by the brief.
- **Dulwich** (Python), **isomorphic-git** (JavaScript): out for the
  same reason, and less complete.
- **git's own `contrib/libgit-rs`**: "Proof-of-concept Git bindings for
  Rust" to `libgit.a`; it reads config and nothing else.
- **gitoxide forks with a server half** (the authors of #2465 and
  #2887): unreviewed, unreleased.

## Assessment

| | libgit2 (git2) | gitoxide (gix) | git (processes) |
|---|---|---|---|
| R1 bare repos | yes | yes | yes |
| R2 reads, listings, last commits | yes | yes, fastest | yes, a process per call or a long-lived `cat-file` |
| R3 commit from changes, CAS | yes | yes | yes (`fast-import` and `update-ref --stdin`) |
| R4 merges, conflicts, restore | yes; merge-base differs from git's | yes; its author doubts its tree merge | yes: merge-ort, the reference |
| R5 history | yes | yes | yes |
| R6 smart HTTP server | **no** (client only) | **no** (declined upstream) | yes, protocol v2 |
| R7 concurrency | a handle per thread; a held lock fails at once | thread-safe handles; locks wait with backoff | processes; locks wait (`core.filesRefLockTimeout`) |
| R8 durability | fsync when asked | **none** | `core.fsync=all` |
| R9 gc, deltas | pack builder with deltas; gc ours | **no deltas**; gc ours | yes |
| R10 streaming | in: spooled; out: loose only | in: spool ours; out: **never** | in and out, with `core.bigFileThreshold` low |
| R11 build, license | C toolchain; GPLv2+exception, LGPL xdiff static | pure Rust; MIT/Apache | a GPLv2 program beside ours |
| R12 future | slow; one maintainer; SHA-256 unstable | fast; one lead and AI agents; monthly breaks | git itself |

## The spike

### Goal

Measure the two libraries on the operations macrofiche needs, with the
git CLI beside them as the reference: speed, peak memory with a 50 MiB
blob, correctness against git's own `fsck`, ref compare-and-swap under
contention, and what a kill -9 leaves behind.

### Method

- **Where.** This machine: AMD Ryzen 9 9950X3D (32 threads), Linux
  7.2.5, btrfs on NVMe, git 2.55.0, rustc 1.99.0. The page cache is warm
  (every read follows its write; dropping it needs root). Nothing ran as
  root; the spike wrote only under `spikes/library/`.
- **The code.** `spikes/library/`, its own cargo project (a workspace of
  one), versions pinned exactly and `Cargo.lock` committed. One trait
  (`src/model.rs`, `Store`) holds the operations; `src/lib_git2.rs`,
  `src/lib_gix.rs`, and `src/lib_git.rs` implement it. Every commit
  has a fixed author, committer, time, and message, so all three must
  produce the same ids; they did.
- **Configs.**
  - `git2`: libgit2, fsync on, an object per loose file;
  - `git2-packed`: libgit2, fsync on, each commit's objects written to
    an in-memory backend and flushed as one pack
    (`git_mempack_write_thin_pack`, then `git_packbuilder_write`);
  - `git2-nofsync`: libgit2, fsync off;
  - `gix`: gitoxide (it has no fsync);
  - `git`: the CLI, `core.fsync=all`, `core.bigFileThreshold=1m`;
  - `git-nofsync`: the CLI, `core.fsync=none`, `core.bigFileThreshold=1m`.
- **The workload** (`cargo run --release -- run <config> 5`). Each step
  runs in a child process of its own, so its peak memory is its own; a
  thread samples `RssAnon` and `RssFile` every millisecond (heap and
  mapped file pages apart: the kernel's `VmHWM` adds them), and for the
  CLI the same is read from git's own process.
  1. init a bare repository;
  2. write a 50 MiB incompressible blob streamed in pieces of unknown
     total size (as an HTTP body arrives), then commit it with 1,000
     small files (20 lines each, under `d0/` to `d9/`) onto `main` from
     in-memory changes, no worktree;
  3. 200 more commits, each changing 5 files, each a CAS on `main`;
  4. read the 50 MiB blob (streamed where the library can), one small
     file through a fresh handle, and 100 small files through one
     handle, each checked byte for byte with its blob id;
  5. the last commit of 100 paths (10 never changed after the first
     commit, so the walk covers all 201 commits), first-parent history
     and a tree diff per commit, checked against the plan;
  6. create `live` at commit 100 (a second create refused), a stale CAS
     refused, then fast-forward `live` to `main` with CAS;
  7. a merge whose two sides changed the same line: the conflict found,
     `main` untouched; then a true merge (an edit, an add, a delete on
     the other side) committed with CAS, its tree checked, and the same
     merge against the now-stale target refused;
  8. gc: libgit2's pack builder over every ref, then the loose copies
     deleted; gix's counter and pack writer, the pack indexed by
     `Bundle::write_to_directory`, then the loose copies deleted;
     `git gc --prune=now`;
  9. step 4 again, now packed; then `git fsck --full --strict`.
- **Contention** (`… contention <config>`): 16 threads commit to one
  branch until each has 20 commits in, retrying on a refused CAS; then
  the first-parent history must be 321 commits and every file present.
  And 1, 8, and 32 threads committing 100 times each, each to a
  repository of its own.
- **kill -9** (`… crash <config> 40`): a writer commits a 1 MiB file and
  a counter in a loop; 40 times its whole process group is killed after
  20 to 420 ms; after each, `git fsck --full` must pass and `main` must
  resolve, and stale locks and temporary files are counted. A stale ref
  lock is reported, shown to block the next writer, then removed. This
  is process death only: the page cache survives, so it tests renames
  and locks, not fsync.
- **The tables below** are printed by `cargo run --release -- table`
  from the evidence files; cells are "median (min–max)" over 5 samples.

### Results

| Step | `git2` | `git2-packed` | `git2-nofsync` | `gix` | `git` | `git-nofsync` |
|---|---|---|---|---|---|---|
| init a bare repo (ms) | 0.55 (0.51–0.59) | 0.56 (0.53–0.66) | 0.52 (0.51–0.59) | 0.99 (0.95–1.21) | 8.55 (8.50–8.84) | 8.50 (8.26–8.66) |
| write the 50 MiB blob, streamed in (ms) | 777 (775–785) | 91 (90–91) | 764 (754–783) | 405 (400–409) | 692 (685–770) | 697 (683–723) |
|   its peak heap (MiB, the git child's for the CLI) | 3.8 | 99.6 (87.6–103.5) | 3.8 | 2.0 | 0.8 | 0.8 |
| 1,000 files and the first commit (ms) | 1933.3 (602.4–4254.6) | 1023.8 (1006.7–1035.5) | 25.6 (25.3–26.1) | 30.5 (29.3–31.6) | 14.6 (13.5–17.9) | 12.7 (12.5–14.0) |
| one commit of 5 files, history (ms, median of 200) | 16.32 (9.24–73.21) | 14.07 (3.75–14.38) | 0.94 (0.93–0.94) | 0.75 (0.74–0.77) | 8.90 (8.90–8.91) | 3.61 (3.59–3.62) |
|   p95 of the 200 (ms) | 18.88 (10.85–101.04) | 22.58 (14.83–23.25) | 1.01 (0.98–1.06) | 0.80 (0.79–0.87) | 10.11 (10.09–10.16) | 4.82 (4.78–4.85) |
| read one small file, fresh handle (ms) | 0.08 (0.07–0.11) | 0.20 (0.07–0.24) | 0.07 (0.07–0.09) | 0.09 (0.08–0.10) | 1.37 (1.31–1.53) | 1.41 (1.33–1.45) |
| read 100 small files, one handle (ms) | 0.83 (0.83–0.85) | 17.04 (9.13–18.66) | 0.82 (0.82–0.86) | 0.88 (0.86–0.93) | 4.51 (3.40–4.70) | 3.48 (3.42–4.62) |
| read the 50 MiB blob, before gc (ms) | 52 (51–53) | 113 (113–114) | 52 (52–53) | 124 (123–124) | 43 (43–44) | 43 (42–45) |
|   its peak heap (MiB) | 0.5 | 50.6 | 0.5 | 100.6 | 0.3 | 0.3 |
|   its peak mapped file pages (MiB) | 54.5 (54.1–54.8) | 55.8 (55.7–55.8) | 54.2 (53.9–54.9) | 58.6 (58.4–58.8) | 54.4 (53.8–54.6) | 54.0 (53.6–54.1) |
| last commit of 100 paths over 201 commits (ms) | 30.7 (30.0–30.8) | 47.9 (47.0–55.8) | 30.8 (30.6–31.1) | 15.8 (15.4–16.0) | 18.8 (18.5–19.5) | 18.7 (18.0–19.4) |
| create a branch (ms) | 3.39 (2.95–6.49) | 5.83 (1.82–7.08) | 0.13 (0.10–0.15) | 0.45 (0.43–0.51) | 2.47 (1.39–4.52) | 1.35 (1.30–1.41) |
| refuse a stale CAS (ms) | 0.073 (0.055–0.101) | 0.066 (0.065–0.083) | 0.044 (0.043–0.058) | 0.022 (0.020–0.023) | 1.812 (1.625–1.908) | 1.699 (1.633–1.788) |
| fast-forward with CAS (ms) | 1.47 (1.16–5.78) | 14.73 (14.62–20.72) | 0.59 (0.52–0.60) | 0.53 (0.50–0.57) | 2.10 (2.07–2.28) | 2.13 (2.02–2.42) |
| merge that conflicts, detected (ms) | 0.51 (0.49–0.54) | 0.64 (0.56–0.67) | 0.47 (0.47–0.55) | 0.32 (0.31–0.40) | 4.11 (3.73–5.33) | 2.66 (2.61–2.84) |
| true merge, committed with CAS (ms) | 7.02 (5.59–20.72) | 44.90 (40.11–48.67) | 3.92 (3.63–4.16) | 0.53 (0.50–0.54) | 5.85 (5.26–6.46) | 5.18 (5.04–6.11) |
| gc / repack (ms) | 1197 (1188–1212) | 1196 (1180–1212) | 1174 (1166–1178) | 881 (877–888) | 187 (184–189) | 173 (169–200) |
|   its peak heap (MiB) | 114.4 (114.3–114.7) | 63.5 (63.5–63.7) | 114.6 (114.5–114.7) | 127.4 (125.5–128.4) | 64.8 (64.3–65.3) | 65.3 (65.2–65.8) |
|   pack after (MiB) | 51.38 (51.37–51.40) | 51.38 (51.38–51.39) | 51.39 (51.39–51.40) | 52.76 | 50.51 | 50.51 |
|   deltified objects of 3,435 | 704 (695–708) | 704 (699–706) | 699 (696–703) | 0 | 2102 (2100–2102) | 2101 (2101–2103) |
| read the 50 MiB blob, after gc (ms) | 114 (113–115) | 113 (112–113) | 113 (112–115) | 47 (45–48) | 43 (42–43) | 43 (43–48) |
|   its peak heap (MiB) | 50.5 | 50.5 | 50.5 | 100.6 | 0.3 | 0.3 |
| read 100 small files, after gc (ms) | 0.59 (0.59–0.61) | 0.58 (0.58–0.61) | 0.59 (0.59–0.60) | 0.53 (0.53–0.54) | 3.40 (3.36–3.49) | 3.41 (3.39–4.82) |
| the big read, before / after gc | stream / in-memory | in-memory / in-memory | stream / in-memory | in-memory / in-memory | stream / stream | stream / stream |
| `git fsck --full --strict` passes | 5 of 5 | 5 of 5 | 5 of 5 | 5 of 5 | 5 of 5 | 5 of 5 |

One set of ids across every sample of every config: the first commit `fd4733a`, the 200th `7af9a44`, the 50 MiB blob `e78ba6e`, the conflicting branch `4592ee5`, main's side `fdaffb7`, and the merge `38ba83c`.

| Contention | `git2` | `git2-packed` | `git2-nofsync` | `gix` | `git` | `git-nofsync` |
|---|---|---|---|---|---|---|
| 16 writers, one branch: commits landed / expected | 321 / 321 | 321 / 321 | 321 / 321 | 321 / 321 | 321 / 321 | 321 / 321 |
|   attempts (refused: moved, locked) | 13365 (2712, 10333) | 22177 (2659, 19198) | 3764 (2221, 1223) | 1345 (867, 158) | 2908 (2414, 174) | 2354 (1968, 66) |
|   commits/s | 244 | 10 | 1706 | 3924 | 73 | 110 |
| 1 writer(s), own repos: commits/s | 285 | 79 | 4210 | 5133 | 151 | 262 |
| 8 writer(s), own repos: commits/s | 168 | 287 | 11751 | 12031 | 834 | 1570 |
| 32 writer(s), own repos: commits/s | 592 | 1006 | 5304 | 7029 | 1011 | 1484 |

| kill -9 | `git2` | `git2-packed` | `git2-nofsync` | `gix` | `git` | `git-nofsync` |
|---|---|---|---|---|---|---|
| rounds / commits on main after | 40 / 430 | 40 / 351 | 40 / 591 | 40 / 1101 | 40 / 250 | 40 / 282 |
| fsck failures or unresolvable main | 0 | 0 | 0 | 0 | 0 | 0 |
| stale ref locks left (each blocked the next writer) | 0 (0) | 2 (2) | 0 (0) | 1 (1) | 0 (0) | 0 (0) |
| temporary files left | 38 | 0 | 35 | 27 | 48 | 42 |

Notes on the tables:

- Heap is peak `RssAnon`. For the CLI it is git's own process's (sampled
  from `/proc/<pid>/status`), except for gc, where `git gc` runs
  `pack-objects` as a grandchild: there it is the children's peak
  `ru_maxrss`, which counts mapped file pages too.
- "Before gc" is loose for `git2`, `git2-nofsync`, and `gix`; already
  packed for `git2-packed` (its first commit's pack) and for the CLI
  (`hash-object` above `core.bigFileThreshold` writes a pack of its own).
- **The durable rows are noisy.** This machine is a desktop in use (a
  file indexer, an inference server, and a browser ran beside the
  spike, with some IO pressure), and fsync on its btrfs volume swings
  with that. The configs' five-sample runs were not simultaneous, so to
  compare durable writes fairly they were also run interleaved, one
  sample each in turn, three rounds (`evidence/interleaved/`):

  | Durable commit of 5 files, median of 200 (p95) | round 1 | round 2 | round 3 |
  |---|---|---|---|
  | `git2` (loose) | 79.1 ms (94.8) | 9.4 ms (11.1) | 9.4 ms (10.8) |
  | `git` | 12.1 ms (14.2) | 8.9 ms (10.0) | 8.9 ms (10.0) |
  | `git2-packed` | 5.1 ms (6.4) | 3.7 ms (4.4) | 3.5 ms (4.3) |
  | First commit, 1,000 files: `git2` / `git` / `git2-packed` | 6,538 / 16 / 997 ms | 627 / 16 / 1,001 ms | 623 / 14 / 1,004 ms |

  Rounds 2 and 3 were calm; round 1 was not, and the libgit2 path that
  syncs a file (and its directory) per object suffered most.
- Contention: 16 writers on one branch, each until 20 of its commits
  were in; then N writers each committing 100 times to a repository of
  its own. Every row of the first block also had all 320 files present
  and `git fsck` clean.

Evidence: `spikes/library/evidence/<config>.json` (every sample, every
step, and a summary), `contention-<config>.json`, `crash-<config>.json`,
`interleaved/<config>-round<n>.json`, `probe-gix-write-blob-stream.json`,
and `probe-git-bigfilethreshold.json`.

### Findings

1. **The same objects, every time.** One set of ids across all 30
   samples of the six configs (the first commit `fd4733a`, the 200th
   `7af9a44`, the 50 MiB blob `e78ba6e`, the merge `38ba83c`), and
   `git fsck --full --strict` clean on every repository. The three agree
   on the trees they build and on this merge: a merge whose sides changed
   one line found the same one conflicted path everywhere, and the clean
   merge produced the same tree.
2. **Durability is the cost that matters, and its shape decides it.** In
   the calm interleaved rounds, a durable commit of five files took 9.4
   ms through libgit2 writing a loose file per object, 8.9 ms through
   git (fast-import packs a large commit and explodes a small one into
   loose objects, `fastimport.unpackLimit` 100), and 3.5 to 3.7 ms
   through libgit2 writing each commit as one pack (`git2-packed`). The
   first commit, 1,000 files: 0.6 s for libgit2 syncing each file, 14 to
   16 ms for git's one pack. Under IO pressure (round 1) the loose
   libgit2 path went to 79 ms a commit and 6.5 s for the 1,000 files,
   git to 12 ms. The catch of a pack per commit: after 200 commits the
   repository held 205 packs, and reading 100 files took 17 ms instead of
   0.8, the last-commit walk 48 ms instead of 31, and a true merge 45 ms
   instead of 4 to 7, until a repack. Whatever writes, macrofiche
   schedules repacks.
3. **In-process is 10 to 20 times faster per call; the calls are small.**
   A read through a fresh handle: 0.07 to 0.09 ms in-process (0.2 with
   205 packs), 1.4 ms through a git process. A stale CAS refused: 0.02 to 0.07 ms against
   1.7 to 1.8 ms. A conflict found: 0.3 to 0.6 ms against 2.7 to 4.1 ms. A
   commit without fsync: 0.75 to 0.94 ms against 3.6 ms. A hundred reads
   through one long-lived `cat-file --batch`: 3.4 to 4.5 ms. Next to a
   durable commit (8.9 to 9.4 ms) and fragment's 30 s deadlines, none of
   it decides anything.
4. **Only git streams a large blob both ways.** Reading the 50 MiB blob:
   git 43 ms with 0.3 MiB of heap, before and after gc (with
   `core.bigFileThreshold=1m`; at git's default of 512m the same packed
   read held 50.3 MiB in `cat-file`: `probe-git-bigfilethreshold.json`);
   libgit2 52 ms with 0.5 MiB while the blob is loose, then 113 to 114
   ms with 50.5 MiB once packed; gix 124 ms loose and 47 ms packed, with 100.6
   MiB of heap both times (twice the blob). Writing it from a stream of
   unknown length: all three spool to learn the size (libgit2 inside
   `git_blob_create_from_stream`, git and gix in the spike's code), with
   0.8 to 3.8 MiB of heap; gix's own `write_blob_stream` held 65.2 MiB
   where the spool held 1.2 MiB (`probe-gix-write-blob-stream.json`).
   Every read that maps the blob also shows about 54 MiB of file pages:
   the page cache's, reclaimable, not heap.
5. **gc is git's.** `git gc` took 187 ms and deltified 2,102 of the
   3,435 objects into 50.51 MiB, of which the 50 MiB blob is 50.02: the
   rest of the history packs to about 0.5 MiB. libgit2's pack builder
   took 1.2 s for about 700 deltas (1.4 MiB beside the blob); gix's
   writer took 0.9 s and made none (2.7 MiB). Neither library drops the
   loose copies or the old packs; the spike does that itself.
6. **Compare-and-swap holds under contention, everywhere.** Sixteen
   writers on one branch, each until 20 of its commits were in: 321
   commits of history, every file present, `fsck` clean, in every
   config. How they lose differs: libgit2 refuses a held ref lock at
   once (10,333 of its 13,365 attempts with fsync were refused as
   locked, not as moved), while gix and git wait for the lock (158 of
   1,345 and 174 of 2,908). git spent the most per attempt: 73 commits a
   second on the one branch, against 244 for libgit2 and 3,924 for gix
   (which never syncs). On repositories of their own, 32 writers made
   592 (git2), 1,006 (git2-packed), 1,011 (git), and 1,484 (git without
   fsync) commits a second; libgit2 and gix without fsync, 5,304 and
   7,029. With fsync on, the disk sets the ceiling.
7. **kill -9 tears nothing, and leaves locks.** In 40 kills per config no
   repository failed `fsck` and `main` always resolved. But a kill inside
   a ref update leaves `refs/heads/main.lock`, and every later writer is
   refused until it is removed: twice for `git2-packed` and once for
   `gix` in the runs kept here (and, in an earlier round whose evidence
   was overwritten, twice for `git2` and once for `git-nofsync`).
   Temporary files are left by all but `git2-packed`, which writes
   nothing until its pack: 27 to 48 per 40 kills (`tmp_object_git2_*`,
   `tmp_obj_*`, the spike's own spools). So macrofiche clears stale
   locks and temporary files at boot, before it serves, when no writer
   of its own can be alive.

## Recommendation

**git, pinned, driven as processes, jailed.** Concretely:

- **The engine.** git built from a release tarball at a pinned version
  (2.55.0 today) into macrofiche's prefix (no curl: a server needs
  none) and run by absolute path: sandcastle's libkrun treatment. The environment is
  cleared; no system or global config (`GIT_CONFIG_NOSYSTEM=1`,
  `GIT_CONFIG_GLOBAL=/dev/null`); no hooks but macrofiche's.
- **Repository config, set at create.** `core.fsync=all` (git's default
  leaves loose objects and refs unsynced), `core.bigFileThreshold=1m`
  (stream blobs above it, and never delta them), `gc.auto=0` and
  `maintenance.auto=false` (maintenance is macrofiche's to schedule),
  `core.logAllRefUpdates=false`, `receive.fsckObjects=true` for later.
- **Writes.** One `fast-import` stream per commit pack (blobs inline from
  the spools, `from` the expected head, onto a temporary ref), then one
  `update-ref --stdin` transaction that moves the branch from the
  expected head (CAS) and drops the temporary ref. Merges: `merge-tree
  --write-tree` (exit 1 lists the conflicts), `commit-tree`, the same
  CAS. Restore: `commit-tree` of the earlier tree onto the head, CAS.
  Branch creation: `update-ref --stdin` `create`.
- **Reads.** A bounded pool of long-lived `cat-file --batch-command`
  processes per active repository for file bytes (streamed) and blob
  ids; `ls-tree -r -l -z` at a commit for listings (immutable, so
  cached by commit id); `log --first-parent` for history.
- **Each git process jailed**, as sandcastle jails its runner: Landlock
  to the repository and its spool (and no network), no new privileges,
  CPU-time and memory limits, a deadline, killed when its client goes.
- **Later, if reads are measured hot:** gitoxide in-process for reads
  only (0.09 ms against 1.4 ms a read here). A reader has no
  durability role, so gix's missing fsync does not matter there; it
  must keep reading what git writes (the files ref backend, SHA-1).

**Not libgit2**, though it is the closest thing to libkrun in shape: it
is a second, slower-moving implementation of the parts git already does
better (merges, gc, streaming), it has no server half, and its durable
writes need a pack-per-commit path built on an unbound libgit2 call to
be fast. It is the fallback if a library is required: it meets R8 today.

**Not gitoxide** as the base: it fails R8 (no fsync), R9 (no deltas, no
gc), and R10 (no streaming reads), its maintainer has declined R6, and
its API breaks every month. It is the right library to read with later.

## Risks

- **A process per call under load.** About 1.4 ms and a fork per
  call. Measured here only to 32 writers on repositories of their own.
  Bounds: the `cat-file` pools, a per-repository queue for writes (it
  also saves the CAS retries), and gix for reads if it comes to that. A
  load test belongs to the next step, not this spike.
- **Text interfaces.** Answers parse plumbing output (documented as
  stable), but errors are classified from exit statuses and stderr
  ("is at X but expected Y", "File exists"). The pinned version and a
  conformance test per message keep a 409 a 409.
- **Shipping GPLv2 git.** A separate program, so aggregation, not
  linking: its source and build script ship with it, as sandcastle
  ships libkrunfw.
- **git's security.** Pinned means bumped by us for every security
  release. The server-side surface is smaller than a client's (no
  worktree, no submodules, no checkout, no hooks but ours), and the
  jail bounds the rest.
- **fsync on the node's filesystem.** The durable numbers are btrfs on
  NVMe; a node on ext4 or XFS will differ, likely for the better. Measure
  there before promising write latency.
- **Locks after a crash.** A stale `*.lock` blocks a branch until removed
  (finding 7). Removing it is safe only when no writer can be alive: at
  boot, before serving, with macrofiche the only writer.
- **Packs and loose objects pile up.** fast-import leaves a pack per
  large commit and loose objects for small ones; without maintenance
  reads degrade (finding 2's 205 packs). Geometric repacking, run when a
  repository is idle, with a prune expiry longer than any write.
- **Two write paths, once smart HTTP comes.** A push lands through
  `receive-pack`, not through the REST path, so the same CAS, the same
  last-commit map, and the same webhooks come from its hooks.
- **The spike's reach.** One machine, a warm page cache, repositories of
  1,000 files and 200 commits, five samples. It ranks the candidates; it
  does not size a node.

## What macrofiche builds on top

Whichever engine, the contract (docs/contract.md) is macrofiche's: the
HTTP routes, JWT verification and scopes, the webhook and its signing.
On git, macrofiche also builds:

- **The commit-pack path.** NDJSON parsed line by line, each
  `blob_chunk` decoded and appended to a spool per `content_id` (one
  chunk in memory at a time, contract section 8), limits on lines, files,
  and bytes; then the fast-import stream and the CAS transaction. An
  in-process queue per repository orders its writers; CAS keeps them
  correct anyway.
- **The last-commit map** (contract section 13): each commit's path to
  last-commit map, from the parent's and the changed paths, recorded when
  the commit is written; for merges and restores, from `diff-tree`. git
  has no such index; the history walk (19 ms for 100 paths over 201
  commits here) is the fallback and the rebuild.
- **Listings and pagination.** A stable order and an opaque cursor over
  `ls-tree` output, cached per commit id.
- **The process layer.** Spawning git with the jail, the deadline, and
  the cleared environment; the `cat-file` pools (spawn, reuse, reap);
  exit statuses and stderr to typed errors (409, 404, 412, 500).
- **Maintenance.** A scheduler that runs `git maintenance run`
  (loose objects, incremental or geometric repack, commit-graph,
  pack-refs) on idle repositories, never `gc --prune=now` beside a
  writer.
- **Boot recovery.** Stale `*.lock` files, temporary objects
  (`tmp_obj_*`, `incoming-*`), leftover `refs/macrofiche/tmp/*`, and
  spools, cleared before serving (finding 7).
- **Repository size** for metering (`count-objects -v`).
- **Later, smart HTTP.** git has the protocol; macrofiche has the HTTP
  half: `GET /info/refs?service=…`, `POST /git-upload-pack`, `POST
  /git-receive-pack`, the `Git-Protocol` header passed through as
  `GIT_PROTOCOL` (version 2), to `git upload-pack --stateless-rpc` and
  `git receive-pack --stateless-rpc` (or `git http-backend` as a CGI);
  authentication and scopes in front; `pre-receive` enforcing the
  branch rules and CAS, `post-receive` updating the last-commit map and
  sending the webhook; `receive.fsckObjects`, git's quarantine, and
  `receive.maxInputSize`. On libgit2 or gitoxide all of the protocol
  itself (refs advertisement, negotiation, pack generation with deltas,
  thin packs, shallow and filters, report-status, sideband) would be
  macrofiche's to write too.

On libgit2, add gc (old packs and loose copies dropped, geometric
repacking), the pack-per-commit write path (`git_mempack_write_thin_pack`
declared by hand; `git_mempack_dump` packs each commit's whole tree,
which the spike's first try did: 205 packs, 10 GiB of them, 5 GiB of
RSS, 202 s for the 200 commits; that run's evidence was not kept), lock
retries, and streaming reads of packed blobs. On gitoxide, add fsync
(inside its tempfile and lock crates, so upstream or a fork), the spool
before every streamed write, a streaming reader, delta compression or
`git gc`, and the server half.

## Sources

Read or measured on 2026-10-03.

- crates.io API: `git2` 0.21.0 (2026-05-18), `libgit2-sys`
  0.18.8+1.9.7 (2026-08-21), `gix` 0.88.0 (2026-09-25) and its version
  history (0.78.0 on 2026-01-22). The GitHub API: commits since
  2025-10-03 for libgit2/libgit2, GitoxideLabs/gitoxide, and
  rust-lang/git2-rs.
- libgit2, from the vendored source of `libgit2-sys` 0.18.8+1.9.7:
  `COPYING` (the linking exception), `deps/xdiff/xdiff.h` (LGPL-2.1+),
  `include/git2/common.h` (`GIT_OPT_ENABLE_FSYNC_GITDIR`, "defaults to
  disabled"), `src/libgit2/odb.c` (`init_fake_wstream`),
  `src/libgit2/odb_loose.c` and `odb_pack.c` (read streams),
  `src/libgit2/odb_mempack.c` (`git_mempack_dump`,
  `git_mempack_write_thin_pack`), `src/libgit2/blob.c`
  (`git_blob_create_from_stream`), `src/libgit2/transports/smart_protocol.c`.
  Releases: https://github.com/libgit2/libgit2/releases. Threading:
  https://github.com/libgit2/libgit2/blob/main/docs/threading.md.
- gitoxide: https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md;
  the September 2026 report, `etc/reports/26-09.md`; pull requests
  https://github.com/GitoxideLabs/gitoxide/pull/2465 and
  https://github.com/GitoxideLabs/gitoxide/pull/2887 (closed unmerged
  2026-08-06); the source of `gix` 0.88.0 (`Repository::write_blob_stream`
  in `src/repository/object.rs`) and of every `gix-*` crate in the
  spike's tree (no `fsync`, `sync_all`, or `sync_data`);
  `gix-pack` 0.75.0 (`iter_from_counts::Mode`).
- git: `git-merge-tree(1)` (https://git-scm.com/docs/git-merge-tree);
  `git-config(1)` from git 2.55.0 (`core.fsync`, `core.fsyncMethod`,
  `core.bigFileThreshold`); `contrib/libgit-rs` in git's tree.
- GitHub, "Scaling merge-ort across GitHub", 2023-07-27:
  https://github.blog/engineering/infrastructure/scaling-merge-ort-across-github/
- GitLab, "Deprecate Libgit2 in Gitaly" (epic 9092):
  https://gitlab.com/groups/gitlab-org/-/epics/9092; "Implement merges
  using the new 'git merge-tree' command":
  https://gitlab.com/gitlab-org/gitaly/-/merge_requests/4479
- CVE-2024-24577 (RUSTSEC-2024-0013): https://rustsec.org/advisories/RUSTSEC-2024-0013;
  CVE-2026-5917: https://ubuntu.com/security/CVE-2026-5917
- go-git v6 backend: https://pkg.go.dev/github.com/go-git/go-git/v6/backend;
  v6.0.0-alpha.5: https://github.com/go-git/go-git/releases/tag/v6.0.0-alpha.5
- cargo's dependencies: https://github.com/rust-lang/cargo/blob/master/Cargo.toml
