# LFS Cloud

LFS Cloud is a planned Git LFS-compatible server and CLI for storing large Git-tracked files outside the Git host's built-in LFS storage.

The initial goal is to keep normal Git version control on GitHub, while routing Git LFS objects through an `lfs-cloud` server to Google Drive storage you control.

> Status: design and scaffold stage. A minimal `serve` command can load config,
> bind an HTTP listener, print reachable URLs, and resolve configured LFS
> routes, requiring a local LFS Cloud session token before parsing authenticated
> requests. The binary now uses a `clap` root command with shared `--config`
> and `--log-level` flags and dispatches `serve` through the server runtime.
> CLI support code can detect the current Git worktree and parse
> GitHub-style HTTPS/SSH remotes, and `lfs-cloud init --server` can resolve
> the current repository's intended Git LFS endpoint, write or update
> `.lfsconfig`, or write only repository-local Git config with `--local`.
> `lfs-cloud login --server` can open the server's GitHub OAuth login URL for
> the current repository, accept the returned local `lfs-cloud` token, and store
> that local token in Git's credential helper for the repository-scoped LFS URL.
> `lfs-cloud status` can inspect the current repository against server config,
> check TCP reachability for the configured server URL, verify that a local LFS
> credential is available, validate the configured Drive credential reference,
> and report local cache directory readiness.
> `lfs-cloud pull` can run a Git LFS fetch for the current checkout, ingest
> fetched objects from the repository's Git LFS media storage into the shared
> cache, and hydrate LFS-tracked pointer files with verified cache bytes.
> Git LFS batch request parsing plus download and upload batch response
> generation are implemented, with GitHub read/write authorization enforced per
> batch operation. Upload batches now check configured storage availability,
> advertise upload actions for missing objects, and accept authenticated object
> `PUT` uploads through temp-file staging, SHA-256 verification, Google Drive
> storage, and metadata recording. Download batches check configured storage
> availability, advertise download actions for existing objects, and stream
> authenticated object `GET` downloads from Google Drive through `lfs-cloud`.
> LFS route, authentication, method, body-size, parse, authorization,
> upload-integrity, local staging-capacity, idle upload timeout, and storage
> failures return Git LFS JSON error payloads with matching HTTP statuses.
> Library helpers can ingest verified objects from existing `.git/lfs/objects`
> caches into the shared local cache, materialize verified cached objects into
> worktree paths with copy-on-write cloning where available and fallback
> copying elsewhere, and hydrate matching Git LFS pointer files from cache
> without overwriting non-pointer worktree content. Clean hydrated files can be
> dehydrated back to Git LFS pointers only after their bytes are verified and
> preserved in the shared cache. `lfs-cloud hydrate <path...>` and
> `lfs-cloud dehydrate <path...>` expose those local cache operations through
> the CLI. `lfs-cloud gc` can remove shared local cache objects that are no
> longer referenced by any registered worktree, with a dry-run mode for review.
> Migration support code can inspect an existing Git worktree for local Git LFS
> installation status, LFS filter config, repository LFS endpoint config, and
> `.gitattributes` patterns that declare `filter=lfs` without mutating local
> state, and can enumerate checked-out Git LFS pointer files for tracked paths
> whose Git attributes use `filter=lfs`. It can also enumerate Git LFS pointer
> blobs reachable from selected refs or all fetched refs by evaluating
> historical `filter=lfs` attributes without checking out those refs.
> Migration planning can also check discovered object identities against local
> Git LFS media storage and an optional shared `lfs-cloud` cache, verifying
> SHA-256 and size before treating local bytes as available. Missing migration
> objects can be fetched from the source Git LFS provider into local media
> storage without changing checked-out worktree files.
> The other commands below are still planned product behavior.

## Why

Git LFS keeps large binaries out of Git history, but hosted LFS storage can be expensive, quota-limited, or hard to clean up. LFS Cloud is intended to provide:

- Google Drive storage for Git LFS objects
- repo-provider-based authorization, so users can read/write LFS objects only when they have matching repo permissions
- lower local disk duplication through a shared cache and copy-on-write file materialization where supported
- migration tooling for existing Git LFS repositories

## Initial Support

The first supported repository provider is:

- GitHub

The first supported storage provider is:

- Google Drive

The architecture is intended to allow future repository providers and storage providers, but those are not part of the initial supported scope.

## How It Works

Git LFS commits small pointer files to the repository:

```text
version https://git-lfs.github.com/spec/v1
oid sha256:<object-hash>
size <bytes>
```

The actual bytes live in LFS storage. LFS Cloud sits between the Git LFS client and the chosen storage provider:

```text
Git LFS client
  -> lfs-cloud server
    -> Google Drive
```

Repository permissions remain the source of truth:

```text
repo read access
  may download LFS objects

repo write access
  may upload LFS objects
```

## Planned Commands

### Run A Local Server

The intended MVP deployment is a local server, optionally reachable on the local network:

```bash
lfs-cloud serve --config ./lfs-cloud.yml --port 8080
```

For LAN exposure:

```bash
lfs-cloud serve --config ./lfs-cloud.yml --host 0.0.0.0 --port 8080
```

The CLI should print addresses like:

```text
lfs-cloud server running
  local:   http://127.0.0.1:8080
  network: http://192.168.1.25:8080
```

### Initialize A Repository

```bash
lfs-cloud init --server http://127.0.0.1:8080
```

This resolves the current repository's `origin` remote and writes or updates
`.lfsconfig` with the `lfs-cloud` endpoint, printing the previous and new
`lfs.url` values. To avoid creating or modifying a committed `.lfsconfig`,
write the endpoint only to repository-local Git config:

```bash
lfs-cloud init --server http://127.0.0.1:8080 --local
```

Example `.lfsconfig`:

```ini
[lfs]
    url = http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs
```

### Authenticate A Repository

```bash
lfs-cloud login --server http://127.0.0.1:8080
```

This opens the running server's GitHub OAuth login URL for the current
repository. After the browser callback returns JSON, paste the `lfs_token`
value into the CLI prompt. The CLI stores that local LFS Cloud token in Git's
credential helper for the repository-scoped LFS URL. It does not store the
GitHub OAuth access token in Git credentials.

### Check Repository Status

```bash
lfs-cloud --config ./lfs-cloud.yml status
```

This checks the current Git repository's `origin` remote against the server
configuration, verifies that the configured server URL is reachable at the TCP
level, confirms that Git's credential helper can return a local `lfs-cloud`
token for the repository LFS URL, validates that the configured Google Drive
credential reference can be loaded, and reports whether the local cache objects
directory already exists. Use `--server` to check a different reachable server
base URL, or `--cache-root` to inspect a non-default local cache root.

### Hydrate Cached Files

```bash
lfs-cloud hydrate path/to/file.bin
```

This replaces matching Git LFS pointer files with verified object bytes from
the shared local cache. It refuses non-pointer worktree content and fails if the
referenced object is missing or corrupt in the cache. Use `--cache-root` to
target a non-default local cache root.

### Dehydrate Files To Pointers

```bash
lfs-cloud dehydrate path/to/file.bin
```

This hashes clean worktree bytes, preserves them in the shared local cache, and
replaces the worktree file with a canonical Git LFS pointer. Existing matching
pointer files are treated as already dehydrated. Use `--cache-root` to target a
non-default local cache root.

### Garbage-Collect The Local Cache

```bash
lfs-cloud gc --dry-run
```

This scans registered worktrees in the shared cache registry, keeps cached
objects referenced by Git LFS pointer files in those worktrees, and removes
unreferenced cached objects. It also prunes missing worktree registrations
during a real run. Use `--cache-root` to target a non-default local cache root.

### Pull And Materialize LFS Objects

```bash
lfs-cloud pull
```

This runs `git lfs fetch`, copies verified fetched objects from the repository
Git LFS media storage into the shared `lfs-cloud` cache, and replaces
LFS-tracked pointer files in the current checkout with verified cache bytes. Use
`--cache-root` to target a non-default local cache root.

### Migrate From Existing Git LFS

```bash
lfs-cloud migrate --server http://127.0.0.1:8080 --all-refs
```

The planned migration command should:

- read existing Git LFS pointer files
- fetch missing objects from the current LFS provider
- upload those same objects to the configured `lfs-cloud` storage provider
- update the repo's LFS URL
- avoid rewriting Git history in the normal case

For GitHub LFS cleanup assistance:

```bash
lfs-cloud migrate --server http://127.0.0.1:8080 --all-refs --purge-source-lfs
```

For GitHub, automatic purge is not expected to be possible through a normal API. The command should instead produce a report and helper text for GitHub Support.

## Server Configuration

The server should use private configuration, not committed repo files, to decide which repository maps to which storage provider.

Example shape:

```yaml
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080
  metadata_path: ./.lfs-cloud/metadata.sqlite3

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: ${GITHUB_CLIENT_ID}
    oauth_client_secret: ${GITHUB_CLIENT_SECRET}

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: 012345abcdef

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
```

String values may reference environment variables with `${NAME}`. This keeps
OAuth client settings and backend credential references out of the YAML file
while still letting validation report the exact missing key.

`server.metadata_path` is optional. When omitted, the server resolves the
SQLite metadata database to `.lfs-cloud/metadata.sqlite3` beside the config
file, keeping routing, object, session, and transfer-attempt state in
server-owned local storage.

For the current server-side Google Drive credential loader, a bare
`credentials_ref` such as `google-drive-user-a` maps to an environment variable
named `LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_GOOGLE_DRIVE_USER_A`. The environment
value is a JSON object containing `client_id`, `client_secret`, `refresh_token`,
and optionally `token_uri`. Custom `token_uri` values must use HTTPS, except
for loopback HTTP endpoints used by local tests and development tools. Use
`env:NAME` as the `credentials_ref` value when the environment variable name
must be explicit.

The MVP Google Drive scope is
`https://www.googleapis.com/auth/drive.file`. The configured `root_folder_id`
must be a folder that the app created, opened through the setup flow, or was
explicitly made accessible to this OAuth client. Current library code can
validate the folder with a non-mutating Drive metadata probe before transfer
paths depend on it.

The `.lfsconfig` file points only to the LFS Cloud endpoint. It should not contain Google Drive, S3, or other backend credentials.

## Deployment Notes

For early use, prefer:

- `localhost` for single-machine development
- LAN exposure for trusted local-network machines
- self-hosted or VPS deployment when remote access is needed

Avoid relying on tunnel services such as ngrok for normal large-file traffic. They are useful for demos or temporary testing, but their bandwidth/request limits are a poor fit for GB-scale LFS usage.

For production, choose hosting where transfer bandwidth is expected, controlled, and affordable. Serverless platforms with small request/response body limits are not a good fit for proxying LFS object bytes.

## Local Disk Usage

Stock Git LFS stores objects in `.git/lfs/objects` and also writes full files into the working tree. LFS Cloud intends to reduce duplication by using:

```text
~/.lfs-cloud/objects/<sha256>
  shared local cache

repo/path/to/file
  copy-on-write clone where supported
```

On APFS and other copy-on-write filesystems, this can allow the cache object and checked-out file to share disk blocks until modified.

## Current State

This repository currently contains planning documents, project configuration, a testable `clap` CLI root with shared config-path and log-level flags, Git worktree detection and GitHub-style remote parsing helpers, `lfs-cloud init --server` Git LFS endpoint resolution plus `.lfsconfig` or local-only Git config writes for the current repository, `lfs-cloud login --server` browser handoff and local LFS token credential-helper storage, `lfs-cloud status` readiness diagnostics for server reachability, repository mapping, local auth, storage credential loading, and local cache directory state, `lfs-cloud hydrate <path...>` and `lfs-cloud dehydrate <path...>` local cache operations, `lfs-cloud gc` registry-based local cache cleanup, typed config loading/validation, SQLite metadata database path resolution and schema migration setup, typed metadata object lookup and verified object upsert helpers, GitHub OAuth authorization URL construction, callback state validation and routing, code-to-token exchange helpers, authenticated GitHub user identity lookup, GitHub repository permission-check helpers, local LFS Cloud session token issuance, Git credential approval and lookup helpers for local LFS tokens, fallback instructions for systems without a configured Git credential helper, server-side Google Drive credential loading, Google OAuth refresh-token exchange helpers, Google Drive root-folder validation helpers, repository-scoped Google Drive object key helpers, Drive object existence lookup helpers, staged-file verification and resumable Drive upload helpers, Drive media download streaming helpers with classified provider errors, local cache path helpers for the planned shared object layout under `~/.lfs-cloud/objects`, verified ingest from existing `.git/lfs/objects` into the shared cache, verified cache-to-worktree materialization with macOS copy-on-write cloning and fallback copying, pointer-file hydration from shared cache without overwriting non-pointer worktree content, clean worktree dehydration back to canonical Git LFS pointers after preserving verified bytes in cache, versioned worktree registrations in `~/.lfs-cloud/worktrees.json` for local cache garbage collection, migration discovery helpers for existing Git LFS configuration plus current-checkout, selected-ref, and all-fetched-ref pointer enumeration, migration transfer helpers for verified local availability checks and source Git LFS fetches that do not update worktree files, and a minimal `lfs-cloud serve` listener that loads config, initializes metadata storage, syncs configured repository/storage parent rows into metadata, reports local/LAN URLs, resolves configured LFS repository routes, requires valid local LFS token authentication, privately retains the GitHub OAuth token server-side for repository permission checks, parses authenticated Git LFS batch requests, enforces GitHub read/write authorization per batch operation, returns Git LFS JSON error payloads for LFS route/auth/method/body-size/parse/authorization/upload-integrity/local-staging/storage failures, generates download batch responses with actions for existing objects, generates upload batch actions after storage availability lookup, accepts authenticated object uploads through guarded temp-file staging, SHA-256 verification, Google Drive storage, and metadata recording, and streams authenticated object downloads from Google Drive through `lfs-cloud`. CLI migration and release packaging behavior have not been implemented yet.

See [IMPLEMENTATION.md](IMPLEMENTATION.md) for architecture details, risks, and open questions.
