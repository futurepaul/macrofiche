# The contract: code.storage's API as fragment uses it

> Read on 2026-10-03 from fragment's `selfhost` branch (commit `0dbb89b`),
> and from code.storage's public reference: the OpenAPI description
> (`code.storage` 1.0.0, https://code.storage/openapi.json) and the docs
> pages on authentication, webhooks, Git operations, Git LFS and the
> ephemeral namespace (https://code.storage/docs/llms.txt lists them).
> Paths without a repository name are fragment's. "The fake" is
> `crates/fakes/src/codestorage.rs`, the in-repo implementation that
> fragment's tests run against. "The service" is code.storage itself.

## Summary

macrofiche answers thirteen HTTP routes and sends one webhook. That is
all of code.storage that fragment uses: the twelve of section 5, and the
merge preview that fragment PR #127 adds (section 15).

- Every client speaks REST over HTTPS with a bearer JWT. Nothing in
  fragment speaks git's smart-HTTP protocol: no clone, fetch or push, no
  LFS, no `git2` (`cli/src/codestorage.rs:1-4`,
  `docs/technical-debt-ledger.md:485-495`). A grep of `cell/`, `cli/`,
  `agent/` and `images/` for `info/refs`, `git-upload-pack`, `git clone`
  and `git push` finds nothing that reaches a fragment repo.
- Commits arrive as NDJSON commit packs with compare-and-swap on the
  branch head. Ref moves are merges, branch creations and restore
  commits.
- Files of 1 MiB or more never reach git as bytes. They arrive as git-lfs
  v1 pointer files of about 133 bytes, and their bytes live in the
  platform's bucket (section 8).
- Object ids must be 40 lowercase hex characters: SHA-1 git (section 4).
- The fake is the conformance reference. Where it differs from the
  service, section 10 says which side macrofiche follows, and section 14
  lists what is still open.

## 1. Clients

| Client | Finds macrofiche at | Token | Calls |
|---|---|---|---|
| The cell (Rust on Workers; on celld when self-hosted) | `CODESTORAGE_API_URL`, default `https://api.<org>.code.storage` (`cell/src/config.rs:263-271`, `crates/core/src/codestorage.rs:110-112`) | signs its own with the org key: `sub` `fragment-runtime`, one scope, 300 s, cached per (repo, scopes) until 60 s before expiry (`cell/src/cs.rs:26-37, 100-108`) | 5.1–5.10 |
| The CLI | the `api` field of `GET /api/f/{name}/storage-token` on the fragment host; `FRAGMENT_CODESTORAGE_URL` or the config's `codestorage` key overrides it (`cli/src/codestorage.rs:105-113`, `cli/src/main.rs:575-582`) | the same answer's `token`: `sub` `editor:<identity hex>`, `git:read` and `git:write`, 900 s (`cell/src/cs.rs:308-322`). Reused for at most 60 s; minted again when less than 120 s is left, or after a 401 or 403 (`cli/src/codestorage.rs:35-40, 175-178, 410-416`) | 5.4–5.6, 5.8–5.12 |
| Computers | the fragment CLI inside the Hermes image (`images/hermes/Dockerfile:26-51`), with a token minted for the agent (`docs/technical-debt-ledger.md:684-700`) | as the CLI | as the CLI |

The host's answer is `{token, repo, api, expiresAt}`, `expiresAt` in
milliseconds (`docs/api.md:484`). `repo` is the url form (section 2). The
CLI calls `{api}/api/repos/{repo}/...`. The self-host plan puts git behind
an intercept (`git.fragment.internal`) so a computer needs no internet
(`docs/self-host.md:155-160`); macrofiche is then reached at that name.

| Client | Deadline | Retries |
|---|---|---|
| Cell | 30 s per call, body included; for a streamed read, 30 s to the headers (`cell/src/cs.rs:20, 76-93`) | none at the transport. A commit retries on 409 up to 5 times (`cell/src/files.rs:29, 118-152`), a deploy up to 5 (`cell/src/publish.rs:26, 91-105`) |
| CLI | connect 15 s; total 30 s plus 1 s per 32 KiB of request body (`cli/src/api.rs:160-166`) | GET and HEAD are retried on a transport failure, 3 attempts. A POST whose answer is lost is never sent again (`OutcomeUnknown`, `cli/src/codestorage.rs:197-218`). CAS loops run 3 attempts (`cli/src/codestorage.rs:24`) |

## 2. Identity: orgs, repo names, the url form

**Org.** A deployment names one org, `CODESTORAGE_ORG`. It is the JWT
`iss`. Several deployments may share an org: `CODESTORAGE_REPO_PREFIX`
keeps their repos apart (`docs/api.md:20-24`).

**Repo name.** `<prefix><label>--<username>` (`cell/src/fragment.rs:1121-1124`,
`crates/proto/src/lib.rs:280-283`):

- `prefix` is empty or `<branch>--`: at most 20 bytes of `[a-z0-9-]`
  (`cell/src/config.rs:266-270`).
- `label` is 1 to 63 bytes of `[a-z0-9-]`, alphanumeric at both ends,
  never `--` (`crates/proto/src/lib.rs:15, 233-242`).
- `username` is at most 32 bytes (`crates/proto/src/lib.rs:18`).

So a name is lowercase ASCII letters, digits and hyphens, at most 117
bytes, with no `/`. Both clients percent-encode the path segment anyway
(`cell/src/cs.rs:60-62`, `cli/src/codestorage.rs:193-195`). macrofiche
decodes it once, as the service does.

Fragment never deletes a repo. A fragment deleted and created again under
the same name finds its old repo and keeps its history and branches
(`cell/src/cs.rs:142-176`, `docs/operate.md:191`).

**The url form.** Creating a repo answers a `repo_id`.
`GET /api/repo-urls/{repo_id}` answers its `url`. The url is what every
repo-scoped path and every `repo` claim names, and what the cell stores
as the fragment's repo (`cell/src/cs.rs:142-183`). The two
implementations disagree on its shape:

- The fake makes the url a UUID-shaped string distinct from the name, and
  answers 404 for the name (fake `5-8, 458-460`). e2e `create` asserts a
  36-character, four-hyphen repo (`crates/e2e/src/lanes/control.rs:209-216`).
- The service documents `url` and `http_url` as deprecated aliases of
  `repo_name`, and `repo_name` is what the `repo` claim names. In org
  `finite`, `url` equals `repo_name` (probed 2026-09-23,
  `docs/phase-3.md:128-131`).

Fragment works with either, because it stores whatever `url` says.
Section 14, question 1.

## 3. Authentication

### Token format

Fragment signs every token the same way (`crates/core/src/codestorage.rs:26-56`):

- Header: exactly `{"alg":"ES256","typ":"JWT"}`, with no `kid`.
- Claims: JSON `{iss, sub, repo, scopes, iat, exp}`.
- Signature: ECDSA P-256 over SHA-256 of `<header b64>.<claims b64>`.
  It is the 64-byte `r‖s`, and all three parts are base64url without
  padding.

| Claim | What fragment puts there |
|---|---|
| `iss` | the org |
| `sub` | `fragment-runtime` (the cell), or `editor:<identity hex>` (a storage token) |
| `repo` | repo-scoped calls: the url form. `POST /api/repos`: the new repo's name. `GET /api/repos`: the org. `GET /api/repo-urls/{id}`: that id (`cell/src/cs.rs:144-182`) |
| `scopes` | an array drawn from `git:read`, `git:write`, `repo:write`, `org:read` (`cell/src/keys.rs:31, 97`) |
| `iat`, `exp` | unix seconds. `exp - iat` is 300 (the cell's own) or 900 (storage tokens), never more than 900 (`cell/src/keys.rs:30, 95-109`, `crates/proto/src/lib.rs:60`) |

| Route | Scope |
|---|---|
| `POST /api/repos` | `repo:write` |
| `GET /api/repos`, `GET /api/repo-urls/{id}` | `org:read` |
| `GET .../branch`, `.../files/metadata`, `.../file`, `HEAD .../file`, `GET .../commits` | `git:read` |
| `POST .../commit-pack`, `.../branches/create`, `.../merge`, `.../restore-commit` | `git:write` |

The cell's tokens carry exactly the one scope their call needs. Storage
tokens carry both git scopes. No scope implies another.

### Verification

macrofiche applies the service's rules (code.storage docs,
`platform/authentication`):

- The signature verifies under a registered public key of `iss`.
  Fragment uses ES256 only; the service also takes ES384, ES512 and
  RS256.
- `exp` is in the future. `iss`, `sub` and `scopes` are present, and
  every scope name is one of the four.
- The route's scope is in `scopes`.
- `repo` is present whenever `scopes` holds `git:*` or `repo:write`.
- On a repo-scoped path, the path's repo equals the `repo` claim
  exactly.
- On `POST /api/repos`, the body's `repo_name`, when present, equals the
  claim.

The fake checks the same things, with different statuses
(fake `365-384, 835-856`):

| Failure | Service | Fake | Fragment's reading |
|---|---|---|---|
| no bearer | 401 `unauthorized` | 401 | CLI: `Auth` |
| bad signature, expired, other org | 403 `forbidden`, detail `Invalid or expired token` | 401 | CLI: `Auth`, mints again |
| scope missing | 403 `insufficient_scope` | 403 | CLI: `Auth` |
| `repo` claim missing | 400 `repo_claim_missing` | 403 | `upstream_failed` |
| path repo ≠ claim | 404 `repository_not_found` | 403 | CLI: `NotFound`; cell: absent |
| `repo_name` ≠ claim | 400 `repo_name_mismatch` | 400 | create fails |

The CLI treats 401 and 403 alike (`cli/src/codestorage.rs:231-232`). The
cell turns any status it does not expect into `upstream_failed`
(`cell/src/cs.rs:51-54`). Either column works for fragment. macrofiche
should follow the service.

### Keys

Fragment holds the org's private key as `CODESTORAGE_PRIVATE_KEY`: PKCS#8
PEM, P-256. A one-line PEM with literal `\n` is accepted
(`crates/core/src/codestorage.rs:13-24`, `docs/api.md:52`).

- The fake takes that same private PEM and verifies with its public half
  (fake `45-50, 859-863`). Without one it verifies nothing; the CLI's
  unit tests run that way.
- The service stores only public keys, registered in its dashboard. A
  `kid` header picks one key; without it, every key of the org is tried.
  Restricted keys also need `iat`, `exp - iat ≤ 3600`, and scopes from
  an allowlist.

macrofiche needs, per org, one or more public keys (SPKI PEM is the
natural form) and the org name. Rotation means adding the new key, then
removing the old one. It checks no clock skew beyond `exp`, as the fake.

## 4. Conventions for every call

- **Paths.** Only the short `/api/...` form. Fragment never calls the
  service's deprecated `/api/v1/...` aliases.
- **Query strings.** Decode as `application/x-www-form-urlencoded`,
  where `+` is a space. The cell encodes with `form_urlencoded`
  (`cell/src/cs.rs:56-58`). The CLI percent-encodes every byte outside
  `[A-Za-z0-9-_./]` (`cli/src/api.rs:510-520`).
- **File paths.** What the cell writes passes `valid_repo_path`:
  non-empty, at most 300 bytes, relative, no empty, `.` or `..` segment,
  no control byte, no backslash (`crates/proto/src/lib.rs:62, 213-219`).
  The CLI's paths come from a folder walk that drops dot-segments,
  top-level `node_modules` and editor droppings (`cli/src/sync.rs:341-352`).
  macrofiche refuses with 400 any path git cannot store.
- **SHAs.** Fragment refuses a branch head, a commit's `new_sha`, a
  merge's `new_sha` or a listed commit's `sha` unless it is exactly 40
  lowercase hex (`crates/core/src/codestorage.rs:114-122, 233-235`,
  `cli/src/codestorage.rs:419-422`). e2e checks that a blob sha is 40
  characters (`crates/e2e/src/lanes/plane.rs:69-73`). So repositories use
  git's SHA-1 object format, with ids written in lowercase.
- **Times.** RFC 3339, UTC.
- **Errors.** RFC 9457 problem details, as the service sends them:
  `application/problem+json`, `{type, title, status, code, detail,
  error}`, where `error` mirrors `detail`. The fake sends `{type, status,
  detail, error}` (fake `213-215`). The CLI shows `detail`, else `error`,
  else the first 200 bytes (`cli/src/codestorage.rs:220-239`). The cell
  keeps the first 300 bytes in its error (`cell/src/cs.rs:51-54`).
- **Statuses the CLI branches on** (`cli/src/codestorage.rs:231-238`):
  401 and 403 mean `Auth`; 404 `NotFound`; 409 `CasRejected`; 412
  `Precondition`; 429 `RateLimited`; any other non-2xx is `Http`. The
  cell's readings are listed per endpoint.
- **Rate limits.** The service answers 429 with `Retry-After` and
  `RateLimit`. Neither client retries a 429: the CLI fails, and the cell
  reports `upstream_failed`.

## 5. Endpoints

### 5.1 `POST /api/repos`: create a repo

- **Scope:** `repo:write`. The `repo` claim is the new repo's name.
- **Caller:** the cell, once, when a fragment is created
  (`cell/src/cs.rs:144-152`, `cell/src/fragment.rs:1121-1136`).
- **Request:** `content-type: application/json`, body
  `{"repo_name": "<name>", "default_branch": "main"}`. The service names
  the repo by the claim. Fragment sends no `base_repo` and no `upstream`.
- **200** (service) or **201** (fake):
  `{"repo_id": "...", "repo_name": "...", "http_url": "...", "message": "repository created"}`.
  Fragment reads only `repo_id`, which must be non-empty
  (`crates/core/src/codestorage.rs:160-163`). It then calls 5.3.
- **409:** the name exists. Fragment looks it up with 5.2.
- **400:** the name is invalid, or `repo_name` ≠ claim.
- **Anything else:** the create fails and no fragment is made.
- **Semantics:** the new repo has no branches. Its default branch,
  `main`, is what a call without `ref` reads.

### 5.2 `GET /api/repos`: list the org's repos

- **Scope:** `org:read`. The `repo` claim is the org. The service makes
  `repo` optional for an `org:read`-only token; the fake requires it
  non-empty.
- **Caller:** the cell, only after a 409 from 5.1 (`cell/src/cs.rs:153-176`).
- **Query:** `limit=100`, then `cursor=<next_cursor>`. Fragment sends no
  `q`.
- **200:** `{"repos": [{"repo_id", "repo_name", "url", "default_branch", "created_at"}], "has_more": true, "next_cursor": "..."}`.
  - Newest first. `limit` defaults to 20 and is capped at 100.
  - The cursor is opaque. A bad cursor is 400.
  - `next_cursor` must be non-empty whenever `has_more` is true
    (`crates/core/src/codestorage.rs:175-178`).
- **What fragment does with it:** it matches `repos[].repo_name` against
  the name and takes that entry's `url` (`crates/core/src/codestorage.rs:170-173`).
  It reads at most 100 pages, so 10 000 repos (`cell/src/cs.rs:23-25`).

### 5.3 `GET /api/repo-urls/{repo_id}`: the url form of an id

- **Scope:** `org:read`. The `repo` claim is the `repo_id`. Any
  non-empty value passes.
- **Caller:** the cell, after a create that answered 200 or 201
  (`cell/src/cs.rs:178-182`).
- **200:** `{"repo_id", "repo_name", "url"}`. Fragment reads `url`, which
  must be non-empty.
- **404:** no such id.

### 5.4 `GET /api/repos/{repo}/branch?name=<branch>`: a branch's head

- **Scope:** `git:read`.
- **Callers:** the cell, on every pin refresh, each attempt of a commit,
  and a deploy (`cell/src/cs.rs:186-193`, `cell/src/plane.rs:216-248`);
  the CLI, in sync, deploy, and a watcher's every pass
  (`cli/src/codestorage.rs:247-256`).
- **Query:** `name` is `main` or `live`. Fragment never reads a
  `preview/*` branch and never sends `ephemeral`.
- **200:** `{"branch": {"name": "main", "head_sha": "<40 hex>", "created_at": "<RFC 3339>"}}`.
  Fragment reads `branch.head_sha`.
- **404:** the branch does not exist. Both clients read this as
  "absent". The cell then clears its pin and the index it keeps of that
  branch (`cell/src/plane.rs:220-228`). Any 404 reads the same way, so
  macrofiche never answers 404 here for a passing failure: it answers
  503.
- **Detail text:** e2e `files` asserts the exact answer for a fresh repo:
  404 with `"detail": "branch not found"` (`crates/e2e/src/lanes/plane.rs:61-64`).
- **Load:** the poll asks twice per pass (main and live). A pass comes
  every 300 s while something outside the platform may be writing, and
  once a day otherwise (`cell/src/plane.rs:35-41, 636-673`).

### 5.5 `GET /api/repos/{repo}/files/metadata`: the tree at a commit

- **Scope:** `git:read`.
- **Callers:** the cell, when a pin moves (`cell/src/cs.rs:196-216`); the
  CLI, in sync and pull (`cli/src/codestorage.rs:261-279`).
- **Query:** `ref=<40-hex commit sha>` (always the head just read, never
  a branch name), `limit=1000`, then `cursor`. Fragment never sends
  `path`, `recursive` or `ephemeral`.
- **200:**

```json
{"files": [{"path": "notes/a.md", "mode": "100644", "type": "blob", "size": 9,
            "last_commit_sha": "<40 hex>"}],
 "commits": {}, "ref": "<ref as asked>", "has_more": false}
```

Rules fragment depends on (`crates/core/src/codestorage.rs:132-158`):

- The listing is recursive: every file at the commit, across the pages.
- An entry whose `type` is not `blob` (a tree, symlink or submodule) is
  skipped.
- A `blob` entry without `path`, `size` or `last_commit_sha` fails the
  whole listing. A missing `mode` reads as `100644`.
- `has_more: true` without a non-empty `next_cursor` fails the listing.
- The pages together name each path exactly once. The cell refuses a pin
  move when its index count differs from the listing's
  (`cell/src/plane.rs:250-283`).
- `size` is the git blob's length. For a pointer, that is the pointer's
  length.
- `last_commit_sha` is defined in section 7.

Bounds and differences:

- The cell reads at most 500 pages (`cell/src/cs.rs:22`), the CLI at most
  1000 (`cli/src/codestorage.rs:28`).
- The service's `limit` defaults to 200, at most 1000, and its `commits`
  maps each `last_commit_sha` to that commit's metadata. Fragment ignores
  `commits` and `ref`.
- The fake lists in bytewise path order; its cursor is the first path of
  the next page (fake `539-554`). Fragment needs no order. The listing is
  of an immutable commit, so any stable order and opaque cursor will do.

### 5.6 `GET /api/repos/{repo}/file?path=<path>&ref=<ref>`: a file's bytes

- **Scope:** `git:read`.
- **Callers:**
  - The cell's `read` takes the whole body, then checks a cap
    (`cell/src/cs.rs:222-231`). It reads `fragment.json` (256 KiB),
    `app.mjs` (1 MiB), `applib/*` (4 MiB together), an app's reads (1 MiB),
    pointer candidates (200 B), an agent's instructions, and a page's
    HTML for its preview card (`cell/src/plane.rs:314, 364, 475-515`,
    `cell/src/files.rs:178-194`, `cell/src/blobs.rs:186-205`,
    `cell/src/agents.rs:202-206`, `cell/src/serve.rs:457-489`).
  - The cell's `stream` pipes the body to the fragment's site and to
    `GET /api/f/{name}/file`, forwarding `content-length`
    (`cell/src/cs.rs:233-242`, `cell/src/plane.rs:832-844`).
  - The CLI's `read_file` takes the whole body: pull, conflict copies,
    content checks (`cli/src/codestorage.rs:281-286`, `cli/src/sync.rs:814-937`).
- **Query:**
  - `path`, form-encoded.
  - `ref`, almost always a 40-hex commit sha: the cell's pin, or the
    CLI's listing head. It is `main` in `commit_single_file`
    (`cli/src/sync.rs:952-980`), and when a listing found no head.
- **200:** the raw bytes, with these headers (fake `664-677`):
  - `content-type: application/octet-stream`;
  - `content-length`, the blob's size, required because the cell
    forwards it;
  - `etag: "<blob sha>"`;
  - `x-blob-sha`;
  - `x-last-commit-sha`.

  The service also sends `accept-ranges` and `last-modified`, and honors
  `Range` and conditional headers. Fragment sends none of them.
- **404:** the path is absent at the ref, or the ref is unknown. Fragment
  reads both as "no such file".
- **503 and other errors:** the cell fails the read (`upstream_failed`)
  and tries again on the next refresh. e2e `ops` holds file reads at 503
  for an "outage": live moves, and the code installs once reads return
  (`crates/e2e/src/lanes/app.rs:102-111`).

### 5.7 `HEAD /api/repos/{repo}/file?path=<path>&ref=<sha>`: a file's identity

- **Scope:** `git:read`.
- **Caller:** the cell only (`cell/src/cs.rs:244-258`), for two things:
  - `stat`: the API's `blobSha` and `lastCommitSha`, and an app's
    `this.files.stat` (`cell/src/plane.rs:865-881`, `cell/src/files.rs:210-216`);
  - the compare-and-swap of `job.files.write`, which compares each
    expected path's blob sha at the head before it commits
    (`cell/src/files.rs:118-128`).
- **200:** an empty body, with these headers:
  - `content-length`, the full blob size, not 0;
  - `x-blob-sha`;
  - `x-last-commit-sha`.
- **404:** the path is absent.

`x-blob-sha` reaches app authors, who pass it back as the expected value
of a write. It is git's blob id: SHA-1 over `blob <len>\0<bytes>`
(fake `184-189`).

### 5.8 `POST /api/repos/{repo}/commit-pack`: write a commit

- **Scope:** `git:write`.
- **Callers:** the cell, for every commit it makes: a mutation's file
  effects, job writes, the files API, templates, generated media
  (`cell/src/files.rs:98-153`); the CLI, for a sync push and for
  `commit_single_file` (`cli/src/codestorage.rs:288-310`).
- **Request:** `content-type: application/x-ndjson`, sent with a
  `Content-Length`; neither client streams it chunked. The service's SDKs
  do stream it (`duplex: 'half'`), so macrofiche accepts both. One
  encoder builds it for both clients (`crates/core/src/codestorage.rs:180-230`):

```
{"metadata":{"target_branch":"main","expected_target_sha":"<40 hex>","commit_message":"fragment sync todo.paul: +1 -1",
 "author":{"name":"fragment/1a2b3c4d","email":"1a2b3c4d@users.fragment"},
 "files":[{"path":"notes/a.md","operation":"upsert","content_id":"c0","mode":"100644"},
          {"path":"old.md","operation":"delete","content_id":"c1"}]}}
{"blob_chunk":{"content_id":"c0","data":"aGVsbG8K","eof":true}}
```

Fields as fragment sends them:

- `target_branch` is always `main`.
- `expected_target_sha` is the head read just before. It is omitted only
  when `main` does not exist yet. macrofiche also accepts
  `expected_head_sha`, the name the service's SDKs send (fake `615`).
- Every file entry has a `content_id` of the form `c<index>`. A delete
  has one too, but streams nothing.
- `mode` is always `100644`.
- No `committer`, `base_branch` or `ephemeral`.
- Each upsert streams its bytes as `blob_chunk` lines:
  - standard base64, padded;
  - each chunk at most 4 MiB decoded (`CHUNK_MAX`), in order;
  - the last chunk has `eof: true`;
  - an empty file is one chunk, `"data": ""`, `eof: true`.
- Fragment sends one file's chunks together, files in metadata order.
  macrofiche should also accept interleaved chunks.

Refusals (fake `556-636`; the service's statuses):

| Fault | Status and body |
|---|---|
| empty body, or line 1 not JSON | 400 problem |
| line 1 not `metadata` | 400 `{"error", "result": {"status": "invalid", "message": "first payload must be metadata"}}` |
| an upsert without `content_id` | 400 `{"commit": null, "result": {"success": false, "status": "invalid", "message": "missing content_id for <path>"}}` |
| a chunk whose `content_id` no upsert names, a delete's included | 400, the same shape, `"unexpected content_id …"` |
| a decoded chunk over 4 MiB | 413 `{"result": {"status": "payload_too_large", "success": false, "message": …}}` |
| an upsert whose stream has no `eof` | 400 `incomplete content stream for <path>` |

The service refuses a chunk for a delete, though its OpenAPI example
streams one (`crates/core/src/codestorage.rs:189-194`). e2e `sync` covers
it: three changed files and one deletion land as one commit
(`crates/e2e/src/lanes/sync.rs:46-65`).

**Compare-and-swap.** When an expected sha is present and differs from
the branch's tip, the answer is 409 and nothing is applied. An empty or
all-zero expected sha means "the branch must not exist". The body:

```json
{"commit": {"commit_sha": "0000…", "tree_sha": "0000…", "target_branch": "main", "pack_bytes": 0, "blob_count": 0},
 "result": {"target_branch": "main", "branch": "main", "old_sha": "<tip>", "new_sha": "<tip>",
            "success": false, "status": "precondition_failed",
            "message": "expected branch head did not match current tip"}}
```

With no expected sha, the commit lands on whatever the tip is, and
creates the branch if it is absent.

**No change.** A pack whose commit would change nothing is refused,
never made as an empty commit: its tree is the tip's (the bytes the
branch holds, deletes of absent paths, no files at all). On a branch it
would create, its tree is empty (deletes alone), or `base_branch`'s when
one is named. The answer is 412, its body exactly the service's, with no
commit and every sha empty:

```json
{"commit": null,
 "result": {"target_branch": "", "branch": "", "old_sha": "", "new_sha": "",
            "success": false, "status": "precondition_failed", "message": "no changes to commit"}}
```

- Added 2026-10-06 from fragment PR #163, which read this answer from a
  preview on 2026-10-05 (`crates/core/src/codestorage.rs`,
  `nothing_to_commit`, and the fake since then). The service's OpenAPI
  lists no 412 for this route.
- It is checked after the compare-and-swap, as the fake orders them: a
  stale pack is 409 whatever it changes. The service's order is not
  pinned down; either way the cell ends at main's tip (a 409 reads the
  head again and rebuilds, and the rebuilt pack is the 412).
- A file's mode counts: a pack that changes only a mode commits (git's
  tree; the fake compares bytes, and fragment writes only `100644`).
- The cell reads it (`result.status` and `result.message`) as "main
  holds these files already" and answers main's tip (`cell/src/cs.rs`,
  `Packed::Unchanged`; `cell/src/files.rs`): a keyed write replayed
  after its answer was lost, a write of the bytes main holds, a
  template's second seed. e2e `templates` checks the first two.
- Nothing moves, so nothing is announced.

**201:**

```json
{"commit": {"commit_sha": "<new>", "tree_sha": "<tree>", "target_branch": "main", "pack_bytes": 1234, "blob_count": 1},
 "result": {"target_branch": "main", "branch": "main", "old_sha": "<old, or 40 zeros>", "new_sha": "<new>",
            "success": true, "status": "ok"}}
```

Fragment accepts 200 or 201, and reads `result.success == true` and
`result.new_sha` (`crates/core/src/codestorage.rs:232-235`).

**Semantics.**

- One commit. Its parent is the old tip; a branch's first commit has
  none. Its tree is the old tree plus the upserts, minus the deletes.
- Deleting an absent path does nothing (fake `304-315`); a pack of
  nothing else changes nothing, and is the 412 above.
- The commit's time is the server's clock.
- The branch move is announced by webhook after the answer (section 6).
- An empty change list never comes from fragment. It changes nothing:
  the same 412.

**How the clients react.**

- The cell, on 409: it reads the head again, checks its expected blobs
  again, rebuilds the pack, and tries up to 5 times
  (`cell/src/files.rs:118-152`). Each write is keyed, so it commits once
  however often it is applied (`cell/src/files.rs:107-109`). e2e
  `effects` makes every try lose (`crates/e2e/src/lanes/app.rs:287-294`).
- The CLI, on 409: it lists again and rebuilds the diff, 3 attempts
  (`cli/src/sync.rs:605-679`).
- The CLI, on a lost answer: it re-reads `main` and never resends blind
  (`cli/src/sync.rs:664-669`, tested at `cli/src/codestorage.rs:480-496`).

### 5.9 `POST /api/repos/{repo}/branches/create`: make `live` or a preview

- **Scope:** `git:write`.
- **Callers:** the cell's first deploy (`cell/src/publish.rs:86-106`,
  `cell/src/cs.rs:276-285`); the CLI's first `deploy`
  (`cli/src/main.rs:1764-1766`) and `deploy --preview`
  (`cli/src/main.rs:1752-1762`).
- **Request:**
  `{"base_ref": "<main's tip sha>", "target_branch": "live", "target_is_ephemeral": false}`,
  or `"target_branch": "preview/<12 hex>"` with `"target_is_ephemeral": true`.
- **200** (service) or **201** (fake):
  `{"message": "branch created", "target_branch": "live", "target_is_ephemeral": false, "commit_sha": "<base sha>"}`.
  Fragment reads `commit_sha`.
- **409:** the branch exists. The cell reads `live` again and goes on.
  The CLI fails the deploy as a conflict. The fake also refuses `main`
  with 409.
- **404:** `base_ref` is unknown.
- **Ephemeral refs:** the service keeps them in a separate namespace
  (`refs/namespaces/ephemeral/…`), invisible to clones and to any read
  without `ephemeral=true`. The fake keeps them with the other branches,
  flagged. Fragment makes previews and never reads them back: "the ref
  IS the preview" (`cli/src/main.rs:1752-1755`). e2e `deploy` checks that
  the preview is ephemeral and at main's tip (`crates/e2e/src/lanes/plane.rs:240-246`).
- **Webhook:** the fake announces the new branch, with `before` all
  zeros.

### 5.10 `POST /api/repos/{repo}/merge`: deploy (move `live` to `main`)

- **Scope:** `git:write`.
- **Callers:** the cell's `go_live`, behind the deploy API and template
  seeding (`cell/src/publish.rs:86-106`, `cell/src/cs.rs:287-304`); the
  CLI's `deploy` (`cli/src/main.rs:1764-1786`). Both call it only when
  `live` exists and is not already at main's tip.
- **Request:**

```json
{"target_branch": "live", "source_ref": "main", "strategy": "ff_prefer",
 "expected_target_sha": "<live's tip>", "commit_message": "deploy todo.paul: a note",
 "author": {"name": "...", "email": "..."}}
```

- **200:** fragment reads `target.new_sha`. The answers differ:
  - fake: `{"target": {"branch", "old_sha", "new_sha", "strategy": "ff" | "merge_commit"}, "result": {"success": true, "status": "ok"}}`;
  - service: `{commit_sha, tree_sha, result: "fast_forward" | "merge_commit" | "no_op" | …, promoted_commits, merge_base_sha, source: {…}, target: {branch, ephemeral, old_sha, new_sha}}`.

  macrofiche answers the service's shape, which also carries
  `target.new_sha`.
- **409:** `expected_target_sha` ≠ live's tip (the fake sends section
  5.8's 409 body). On the service, 409 can also mean a merge conflict.
  The cell reads `live` again and retries (5 times); the CLI does the
  same (3 times).
- **404:** the source or the target is unknown.
- **Semantics:** when live's tip is main's tip or an ancestor of it, live
  fast-forwards to main's tip. Otherwise live has diverged; it does so
  only after a rollback's restore commit. Then the fake writes a merge
  commit with parents `[live tip, main tip]` and **main's tree**
  (fake `717-740`), because a deploy means "serve main". The service
  does a real merge here. Section 14, question 3.

### 5.11 `POST /api/repos/{repo}/restore-commit`: roll back `live`

- **Scope:** `git:write`.
- **Caller:** the CLI's `rollback` (`cli/src/main.rs:1194-1210`,
  `cli/src/codestorage.rs:343-361`).
- **Request:** `content-type: application/x-ndjson`, one line and a
  newline:

```json
{"metadata": {"target_branch": "live", "base_ref": "<sha to restore>", "expected_target_sha": "<live's tip>",
              "commit_message": "rollback todo.paul to <sha>", "author": {"name": "...", "email": "..."}}}
```

- **201:** the commit pack's answer shape (5.8). Fragment reads
  `result.success` and `result.new_sha`.
- **412:** `base_ref` is not an ancestor of the target's tip. The fake
  checks first-parent ancestry, and counts the tip itself
  (fake `742-768`). The service also answers 412 when the branch already
  points at `base_ref`, or when the restore would change nothing. The
  CLI reports it as `Precondition`.
- **409:** `expected_target_sha` ≠ tip. The fake checks this after
  ancestry.
- **404:** the target branch is absent.
- **Semantics:** a new commit on the target whose tree is `base_ref`'s,
  and whose first parent is the old tip. The fake also records `base_ref`
  as a second parent (fake `316`). macrofiche writes a single parent;
  fragment's history reads walk first parents, so they are unaffected.

### 5.12 `GET /api/repos/{repo}/commits?ref=live&limit=30`: deploy history

- **Scope:** `git:read`.
- **Callers:** the CLI's `drafts` and `rollback` (`cli/src/main.rs:1194-1230`,
  `cli/src/codestorage.rs:363-377`).
- **200:** `{"commits": [{"sha", "message", "author_name", "author_email", "date", "parent_shas"}], "has_more": false}`.
  The service adds `committer_name`, `committer_email` and `next_cursor`.
  - Every `sha` must be 40 hex.
  - Fragment prints `message`, `author_name` and `date` as they come.
- **Order:** newest first. Element 0 must be the ref's tip, since
  rollback sends it as `expected_target_sha`. Element 1 is the default
  rollback target.
  - The fake walks first parents from the ref (fake `679-694`).
  - The service orders by commit date over every reachable commit.
  - Section 14, question 4.
- **Paging:** fragment sends no cursor. The fake's `limit` defaults to 30,
  at most 100; the service's defaults to 20.

## 6. The push webhook

**When.**

- After every branch move: a commit pack, a branch creation, a merge, a
  restore commit (and a git push, should macrofiche ever take one).
- One delivery per moved branch, and none when `before == after`.
- After the HTTP answer. The writer may see the delivery before it reads
  its answer, or after (fake `386-409`). The cell holds its plane lock
  across its own commit so that an early delivery finds the commit
  recorded (`cell/src/files.rs:137-145`).

**Where.** To each URL registered for the repo. Fragment's URL is
`<platform base>/api/f/<fragment name>/webhook`, and its secret is the
fragment's `webhookSecret`: 32 hex characters, answered by create and by
`rotate` (`cell/src/fragment.rs:1139-1149`, `cell/src/members.rs:559-582`).
The platform forwards that route without a request signature
(`cell/src/lib.rs:1073-1077`).

**Request.** `POST`, with headers `content-type: application/json`,
`x-pierre-event: push` and `x-pierre-signature: t=<unix s>,sha256=<hex>`.
The service also sends `user-agent: Pierre-Webhook/1.0`.

The fake's body (fake `331-347`):

```json
{"repository": {"id": "<repo_id>", "url": "<url form>"}, "ref": "refs/heads/main",
 "before": "<40 hex, or 40 zeros>", "after": "<40 hex>", "customer_id": "fragment-dev",
 "pushed_at": "2026-10-03T12:00:00Z"}
```

The service's: `{"type": "push", "repository": {"repo_name"}, "ref",
"before", "after", "org", "pushed_at"}`.

What the cell reads (`crates/core/src/webhook.rs:36-61`):

- `ref`, which must start with `refs/heads/`;
- `before` and `after`;
- for deduplication, `repository.url` and `pushed_at`.

macrofiche sends the union of the two bodies:
`repository: {repo_id, repo_name, url}`, `type`, `org`, `ref`, `before`,
`after`, `pushed_at`.

**Signature.**

- `hex(HMAC-SHA256(secret, "<t>.<raw body>"))`, lowercase, where `t` is
  the send time.
- The cell refuses a delivery when `|now − t| > 300 s`
  (`crates/core/src/webhook.rs:10-25`, `crates/proto/src/lib.rs:64`).
- Test vector (fake `1192-1201`): body
  `{"ref":"refs/heads/main","before":"0","after":"1"}`, secret `s3cret`,
  `t=1790000000` gives
  `t=1790000000,sha256=4d38e01ea6039c2094e98395081b85c0efd6af8ae2740ebfab403ded420cb4b8`.

**The cell's answer.** 200 `{ok, interpreted, redelivery}`, or
`{ok, ignored}` for another event; 401 for a bad or stale signature
(`cell/src/plane.rs:699-737`).

**Retries and guarantees.**

- The fake tries 3 times, 300 ms apart, signing each attempt again;
  success is any 2xx (fake `782-796`). The service retries "automatically"
  on a schedule it does not publish.
- Every attempt carries a fresh `t`.
- Delivery is at least once and best effort.
- The cell keeps deduplication keys for 7 days. On a delivery it moves
  its pin to the head it reads then, never to `after`, so duplicates and
  reordering are harmless.
- A lost delivery is caught by `refresh` or the poll; e2e `files`
  proves both (`crates/e2e/src/lanes/plane.rs:119-127`).

**Registration.** There is no HTTP route for it today:

- The fake has a lever, `register_webhook` (fake `990-999`), which the
  e2e calls for each fragment it creates (`crates/e2e/src/main.rs:536-547`).
- The dev stack registers none.
- The service has one org-wide subscription, made in its dashboard with
  one secret. Fragment does not use it yet; pins on the hosted fleet move
  by the CLI's `refresh`, the cell's own commits, and the poll
  (`docs/phase-3.md:127-136`).

Section 14, question 2.

## 7. Branches, commits, last commits

| Branch | Written by | Read by |
|---|---|---|
| `main` | every commit: the cell, the CLI, computers | the cell's `main` pin (files, the manifest), sync |
| `live` | 5.9 (first deploy), 5.10 (deploy), 5.11 (rollback) | the cell's `live` pin (the site, the app's code), `drafts`, `rollback` |
| `preview/<12 hex>` | 5.9, ephemeral | nobody |

Fragment never deletes a branch, never writes tags or notes, and never
writes any other branch (`cell/src/plane.rs:42`, `cli/src/codestorage.rs:16-20`).

**Authors and messages.** No committer is sent, so the committer is the
author.

| Writer | Author | Messages |
|---|---|---|
| Cell | the principal's display form (an `npub1…`, or an id as it is), `<that, at most 64 chars>@fragment.invalid` (`cell/src/files.rs:54-60`) | `<op> <id>` (`cell/src/channels.rs:594`); `<op> run <id>: write|remove <path>` (`cell/src/jobs.rs:933`); `<op> run <id>: generated <path>`; `start from the <t> template`; `write N file(s)`; `deploy <name>[: <note>]` (`cell/src/publish.rs`) |
| CLI | `fragment/<8 hex>`, `<8 hex>@users.fragment` (`cli/src/codestorage.rs:138-143`) | `fragment sync <name>: +N -M`; `deploy <name>[: <note>]`; `rollback <name> to <sha>`; `manifest-set <name>` |

**`last_commit_sha`.** It appears in 5.5 and as `x-last-commit-sha` in
5.6 and 5.7.

- The service defines it as the most recent commit reachable from `ref`
  that modified `path`: git's `log -1 -- <path>`, with its TREESAME rule
  through merges.
- The fake records the commit that last wrote the path in that lineage.
  An upsert of unchanged bytes counts, and a merge or restore keeps the
  entries of the tree it takes (fake `304-315`).

Fragment needs two properties:

1. At two commits, an equal `(size, mode, last_commit_sha)` for a path
   means equal bytes. The cell's index follows a branch by these fields
   alone (`crates/core/src/tree.rs:37-53`).
2. A commit that changed a file is that file's last commit. The CLI
   derives its listing after its own commit by this rule instead of
   listing again (`cli/src/sync.rs:763-782`).

Git's definition meets both, because fragment upserts only changed
bytes. macrofiche uses git's definition, computed when the commit is
written (section 13).

## 8. Sizes, large files, pointers

**Large files are pointers.** Bytes of 1 MiB or more never reach git.
The CLI uploads them to the fragment host's blob store and commits a
pointer (`cli/src/blobs.rs:44-61`); the cell does the same for generated
media (`cell/src/ai.rs:238-252`). The host announces the threshold as
`blobMinBytes`, 1 MiB (`cell/src/fragment.rs:1255`,
`crates/core/src/blob.rs:7`). Only an older host announces none, and
then gets raw bytes of any size.

**The pointer** is git-lfs v1 text, exactly
`version https://git-lfs.github.com/spec/v1\noid sha256:<64 hex>\nsize <decimal>\n`
(`crates/core/src/blob.rs:28-33, 63-74`):

- 132 to 134 bytes for the sizes allowed, 1 MiB to 256 MiB
  (`crates/proto/src/lib.rs:163`).
- Fragment commits no `.gitattributes`.
- The bytes live in R2 on Cloudflare, or in celld's bucket when
  self-hosted (`docs/self-host.md`, seam 6). They are never in
  code.storage's LFS store. To macrofiche a pointer is an ordinary small
  blob; macrofiche does not interpret it.
- After every pin move, the cell reads each changed file of 126 to 200
  bytes to find pointers (`cell/src/blobs.rs:27-46, 186-205`).

**Raw bytes that reach git:**

| Writer | Per file | Per commit |
|---|---|---|
| CLI sync | < 1 MiB | no limit: one pack carries the whole diff of a folder. The `notes` template is 79 files and 3.1 MB |
| `POST /api/f/{name}/files` | ≤ 1 MiB | ≤ 1 MiB and ≤ 16 files (`cell/src/publish.rs:30, 108-139`) |
| An app's mutations, job writes | ≤ 256 KiB | ≤ 256 KiB and ≤ 16 files (`crates/proto/src/lib.rs:151-154`, `cell/src/files.rs:86-92`) |
| Generated media | < 1 MiB | 1 file |
| Templates | ≤ 60 KB | ≤ 160 KB (`templates/`) |

**Streaming.**

- *Writes.* base64 adds a third, so a CLI pack can run to tens of MB.
  macrofiche parses a pack line by line and writes each chunk as it
  arrives, holding at most one decoded chunk (4 MiB) per line. A
  `blob_chunk` line is at most about 5.6 MB. Today every file is under
  1 MiB, so each upsert is one line.
- *Reads.* `GET file` streams. The cell pipes it through with its
  `Content-Length`. The CLI reads whole bodies within its 30 s.
- Uploads may be as slow as 32 KiB/s: that is the CLI's own deadline
  (`cli/src/api.rs:160-171`). macrofiche's server timeout must not be
  shorter.

## 9. Concurrency and idempotency

- **Writers.** Many write one repo at once: the cell (which serializes
  its own commits under a per-fragment lock, `cell/src/files.rs:137-145`),
  any number of CLIs and watchers, and agents on computers. Many repos
  are written in parallel.
- **Atomic ref updates.** A branch update is a compare-and-swap on the
  ref. Of two packs naming the same expected head, exactly one lands; the
  other gets 409 with nothing applied (`cli/src/codestorage.rs:444-478`).
  The same holds for `expected_target_sha` on merge and restore, and for
  branch creation: one creator wins, the other gets 409.
- **No idempotency keys.** Fragment makes its writes idempotent itself:
  the cell keys its commits, and the CLI re-reads after a lost answer.
  So macrofiche makes a commit durable and visible before it answers.
  A pack whose answer is lost is on the branch when the client re-reads
  (the fake's `drop_commit_answers`; `cli/src/sync.rs:1315-1339`).
- **Read after write.** A `GET branch` after a 2xx write sees the new
  tip. Reads at a commit sha are immutable and may be cached forever.
- **Webhooks** go out after the answer, outside any lock that writers
  wait on.

## 10. Where the fake and the service differ

| Topic | Fake | Service | macrofiche |
|---|---|---|---|
| url form | UUID-shaped, ≠ name | = `repo_name` | question 1 |
| create, branch create status | 201 | 200 | either; fragment takes both |
| auth failure statuses | 401, 403 | 401, 403, 400, 404 (section 3) | service |
| problem body | no `code`, no `title` | `code`, `title` | service |
| commit ids | synthetic SHA-1 of a counter; `tree_sha` = commit sha | real git | real git |
| `last_commit_sha` | last write, unchanged bytes included | git's rule | git's |
| merge when diverged | merge commit with the source's tree (git's three-way merge after PR #127) | a real merge; 409 on conflict | service (section 15) |
| restore commit parents | old tip and base | not documented | old tip only |
| restore to the tip, or with no change | 201 (412 after PR #127) | 412 | service |
| a commit pack that changes nothing | an empty commit (412 after PR #163) | 412 `no changes to commit` | service |
| commit history | first parents, `limit` 30 | date order, `limit` 20 | question 4 |
| ephemeral refs | flagged in the branch map | own namespace | service |
| `files/metadata` default `limit` | 1000 | 200 | service |
| webhook body | `repository.{id,url}`, `customer_id` | `type`, `repository.repo_name`, `org` | the union |
| webhook registration | lever | dashboard, one per org | question 2 |
| without an org key | any bearer passes | never | never |

## 11. What the service offers that fragment does not use

- **Routes:** repo get, update and delete; `/api/meta`;
  `/api/repo-changes`; branches list, delete and diff; commit get, diff,
  `diff-commit`, `reset-commits`; files list, archive, blame, grep; tags;
  notes; merge preview; git credentials; `pull-upstream`; every
  `/api/v1/...` alias.
- **Features:** forks (`base_repo`) and upstream sync with `repo.sync.*`
  webhooks; ref policies inside JWTs; commit signing; restricted keys,
  `kid`, ES384, ES512 and RS256; `Range` and conditional reads; custom
  webhook headers; rate-limit headers.
- **Git over HTTPS:** clone, fetch and push at
  `https://t:<JWT>@<org>.code.storage/<repo>.git`; the `+ephemeral`
  remote; the import namespace; Git LFS through its batch API at
  `<repo>.git/info/lfs/objects/batch`. Fragment uses none of it.

## 12. Checklist

| # | Endpoint | Cell | CLI and computers | e2e sections that exercise it |
|---|---|---|---|---|
| 5.1 | `POST /api/repos` | create | — | `create`, and every section that creates a fragment |
| 5.2 | `GET /api/repos` | create, after 409 | — | `create` (150 newer repos, then a name created again: `control.rs:276-281`) |
| 5.3 | `GET /api/repo-urls/{id}` | create | — | `create`, and every create |
| 5.4 | `GET .../branch` | pins, commits, deploys, poll | sync, deploy, watch | `files`, `sync`, `watch`, `site` (counts its calls), `deploy` |
| 5.5 | `GET .../files/metadata` | pin moves | sync, pull | `files`, `sync`, `deploy`, `blobs` |
| 5.6 | `GET .../file` | reads, streams | pull, conflicts | `files`, `ops` (503 outage), `site`, `sync`, `appfiles`, `blobs`, `deploy` |
| 5.7 | `HEAD .../file` | stat, job CAS | — | `files` (stat), `appfiles` (conflicts) |
| 5.8 | `POST .../commit-pack` | every own commit | sync, `manifest set` | `appfiles`, `effects` (409 every try), `sync` (one pack), `templates`, `deploy`, `blobs`, `ai`, `brain`, `ledger` |
| 5.9 | `POST .../branches/create` | first deploy | first deploy, `--preview` | `deploy`, `templates` |
| 5.10 | `POST .../merge` | deploy API, templates | deploy | `deploy`, `templates`, `isolation`, `ledger` |
| 5.11 | `POST .../restore-commit` | — | rollback | `deploy`; CLI unit tests |
| 5.12 | `GET .../commits` | — | drafts, rollback | `deploy`; CLI unit tests |
| 6 | push webhook (out) | receives | — | `files` (signature, window, redelivery, lost delivery), and every section using `Suite::commit` locally |

The e2e also drives the fake through levers that are not HTTP routes
(fake `892-1085`): `seed_filler` (`create`), `fail_file_reads` (`ops`),
`sabotage_commit_packs` (`effects`), `silent_commit` (`files`, `sync`),
`external_commit` and `set_branch` (another writer, and a deploy:
`crates/e2e/src/main.rs:560-592`), `register_webhook`, and reads of the
fake's state (`branch`, `file_at`, `is_ephemeral`, `repo_url`,
`requests`, `take_requests`). The CLI's unit tests also use
`drop_commit_answers` and `arm_race`. The hosted lane already runs these
sections without levers: it writes through the platform's routes and
skips each check that reads the fake (`crates/e2e/src/main.rs:536-592`,
`crates/e2e/src/lanes/plane.rs:226-246`).

## 13. What a real git store adds beyond the fake

- **Real objects.** Commit, tree and blob ids are git's SHA-1. The fake
  invents commit ids from a counter and reports a commit's own id as its
  tree.
- **Last commits by history.** Computing `last_commit_sha` for a whole
  tree is a history walk. A store can instead record each commit's
  path-to-last-commit map when it writes the commit (the parent's map,
  plus the changed paths; merges by git's TREESAME rule), so a listing
  costs no walk. Every write goes through macrofiche, so the map is
  always complete.
- **Storage.** Objects in packfiles with deltas and garbage collection.
  The fake keeps every blob ever written, base64, in one JSON file.
- **Durability.** Objects are synced to disk before the ref moves, refs
  update atomically, and a restart loses nothing acknowledged. The fake
  rewrites its JSON file after each write, through a temporary file and
  a rename, without fsync (fake `356-363`).
- **Concurrency.** A lock or CAS per ref, repos in parallel, reads
  without locks. The fake serializes everything behind one mutex
  (fake `155-164`).
- **Reach.** TLS, any interface, authentication always on. The fake
  binds loopback, and with no key it accepts any bearer.
- **Streaming.** Packs are parsed as they arrive and files are served as
  streams. The fake buffers whole requests and responses.
- **Smart HTTP.** Not needed now. With it, a computer's agent could
  clone, push and use git as it already knows how
  (`templates/skills/` teaches `git push`). A push would then need the
  same ref CAS rules and the same webhooks.
- **Size reporting.** A repo's size could be reported, so fragment can
  meter git storage (`docs/technical-debt-ledger.md:405-417`).

## 14. Open questions

1. **The url form.** `url` = `repo_name`, as the service does, or an
   opaque id, as the fake does? e2e `create` asserts the fake's UUID
   shape.
2. **Webhook registration.** The cell has a secret per fragment, but
   neither the fake nor the service registers one over HTTP. Options:
   - a macrofiche extension route (`PUT /api/repos/{repo}/webhook
     {url, secret}`) that the cell calls at create and rotate;
   - an org-wide hook with a route on the cell that dispatches by repo;
   - none, relying on `refresh` and the poll, as the hosted fleet does
     today.
3. **Merging into a diverged `live`.** Should the merge commit take
   main's tree (the fake: "deploy means serve main"), or merge three
   ways as the service does? A three-way merge after a rollback keeps
   the rolled-back changes reverted, and may answer 409 `merge_conflict`,
   which fragment reads as "live moved" and retries until the deploy
   fails. e2e `deploy` does not look at a file the rollback reverted
   after the next deploy.
   *Settled by fragment PR #127 (section 15): the service's three-way
   merge. A deploy previews the merge, and uses restore commits where a
   merge alone would keep a rollback's reverts.*
4. **Commit history order.** First parents, as the fake (what
   `rollback`'s default target assumes), or commit-date order over every
   reachable commit, as the service? They agree on linear history.
5. **Large files.** Keep the pointers, with bytes in the bucket, or let
   macrofiche hold large blobs, by raising the threshold or serving an
   LFS batch API? Either way, what limit does a commit pack get? Today
   it has none.
6. **Test levers.** Does macrofiche expose a test-only admin surface for
   the e2e levers, or does the e2e run against macrofiche under the
   hosted lane's rules? Either way, the exact detail strings the e2e
   asserts (`branch not found`) become part of the contract.
7. **Keys.** Configure per-org public keys in a file (SPKI PEM), or
   derive them from the private key the self-hosted operator already
   holds?
8. **Unsigned mode.** The CLI's unit tests run the fake with no key.
   Does macrofiche need a dev mode that accepts any bearer, or do those
   tests keep the fake?

## 15. The merge preview (from fragment PR #127)

> Read on 2026-10-03 from fragment PR #127 (branch
> `claude/deploy-after-rollback`, commit `5d784b3`) and from
> code.storage's OpenAPI (`repos-previewMerge`, `ReposPreviewMergeResponse`,
> `ReposMergeRequest`, `MergeConflict`, `ReposCommitsRestoreResponse`).
> Paths here are that branch's.

PR #127 makes a deploy after a rollback serve main against the service's
three-way merge. Merging main into a `live` that holds a rollback's
restore commit does nothing while main is unchanged (`no_op`), keeps what
the rollback reverted once main moves on, and conflicts where main
changed a file the rollback reverted. So a deploy no longer merges blind.
It previews the merge, and follows `fragment_core::codestorage::Promotion`
(`crates/core/src/codestorage.rs`):

| The preview answers | The deploy does |
|---|---|
| `source_tip_sha` = `target_tip_sha` | nothing (`Current`) |
| `result: fast_forward` | 5.10, which fast-forwards |
| `result: no_op` (main is in live: a rollback, main unchanged since) | 5.11, a restore commit of main's tip; a 412 is "live's files are main's already" when live has not moved |
| `result: merge_commit` (diverged: a rollback, then main moved) | 5.11, a restore commit of the merge base, then 5.10, which takes main's side of every path; a 412 from the restore is "no change" |

Every step carries the `live` the preview read as `expected_target_sha`.
The cell holds its plane lock across the steps (`cell/src/publish.rs:85-106`);
the CLI cannot (`cli/src/main.rs:1772`).

### 15.1 `GET /api/repos/{repo}/merge/preview`: what a merge would do

- **Scope:** `git:read`.
- **Callers:** the cell's `promote_live` (`cell/src/cs.rs`, behind the
  deploy API and template seeding) and the CLI's `promote_live`
  (`cli/src/codestorage.rs`, behind `deploy`).
- **Query:** `source_branch=main&target_branch=live`, always
  (`DEPLOY_PREVIEW_QUERY`). The service also takes `source_is_ephemeral`,
  `target_is_ephemeral` and `include_content`; fragment sends none.
- **200:** a conflicted preview is 200 too, with `status: conflicted`.

```json
{"status": "clean", "result": "merge_commit",
 "source_branch": "main", "target_branch": "live",
 "source_tip_sha": "<40 hex>", "target_tip_sha": "<40 hex>",
 "merge_base_sha": "<40 hex>", "conflict_paths": [], "conflicts": []}
```

  - `status` is `clean` or `conflicted`; `result` is `merge_commit`,
    `fast_forward` or `no_op`.
  - `conflicts` holds inline blobs (`path`, and `base`, `ours`, `theirs`,
    `result`, each `{oid, content, binary, truncated}`) only with
    `include_content=true`; `filtered_conflicts` names those left out.
  - `merge_base_sha` is git's merge base: the source's tip for `no_op`,
    the target's for `fast_forward`.
- **What fragment reads** (`promotion`): `target_tip_sha` and
  `source_tip_sha`, each 40 hex; `result`; and `merge_base_sha`, 40 hex,
  when `result` is `merge_commit`. Anything else fails the deploy as a
  malformed answer.
- **404:** either branch is absent. **400:** unrelated histories (the
  fake's reading).
- **Semantics:** it writes nothing and moves no ref.

### 15.2 What fragment now relies on in 5.10 and 5.11

- **Merge (5.10)**, the service's semantics:
  - `strategy` is `merge`, `ff_only` or `ff_prefer`. Fragment sends
    `ff_prefer`: fast-forward when it can, else a merge commit.
  - `no_op` when the target holds the source already; a fast-forward
    when the source holds the target; else git's three-way merge from
    the merge base.
  - 409 has its own body, `MergeConflict`:
    `{"error", "conflict_type", "conflict_paths", "merge_base_sha"}`.
    `conflict_type` is `merge_conflict` (the sides conflict),
    `target_moved` (a stale `expected_target_sha`), or `repo_changed`.
    Fragment tells a conflict from a moved `live` with `merge_conflicted`
    (`conflict_type` or `code` equal to `merge_conflict`), and fails a
    conflict instead of retrying it.
  - The answer's `target.new_sha` is still what fragment reads.
  - A squash (`squash: true`) whose tree is the target's would be a
    one-parent commit that changes nothing. macrofiche refuses it with
    5.8's 412 body, and moves nothing. The service's documents do not
    say; fragment never squashes. A merge commit whose tree is the
    target's is still made: its second parent records the merge, as
    git's does, and the fake makes one too.
- **Restore (5.11)**, now also called by every deploy after a rollback:
  - 412 when the branch already points at `base_ref`, when `base_ref` is
    not an ancestor of the tip (an unknown one included), or when the
    restore would change nothing (equal trees). The body is the restore
    result with `result.status` `precondition_failed`.
  - A deploy reads that 412 as "no change"; `rollback` still reports it
    as `Precondition`.
  - The deploy's first step after a rollback carries the message
    `<deploy message> (first, live back to <12 hex>: the last of main it holds)`
    (`back_to_base_message`).
- **The checklist (section 12) gains a row:** 15.1, the cell's deploy API
  and template seeding, the CLI's `deploy`; e2e `deploy` now looks at the
  page a rollback reverted after the next deploy.
