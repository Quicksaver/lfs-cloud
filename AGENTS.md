# AGENTS.md

This file provides context for AI agents working in this codebase.

## Project Overview

**LFS Cloud** is a planned Git LFS-compatible server and CLI for moving Git LFS object storage away from the Git host while keeping normal Git repository hosting unchanged.

The initially supported shape is:

- Git repositories remain on GitHub.
- Git LFS clients point at an `lfs-cloud` endpoint through `.lfsconfig` or local Git config.
- `lfs-cloud` authorizes users through the repository provider's permissions.
- Actual large-file bytes are stored in Google Drive.
- The local CLI reduces disk duplication by using a shared content-addressed cache and copy-on-write materialization where supported.

Future repository providers may include GitLab, Bitbucket, and self-hosted Git services. Future storage providers may include local filesystem, S3-compatible storage, R2, B2, and MinIO. Keep the abstraction boundaries in place, but do not present future providers as initially supported.

This repository is currently in planning/early scaffold state. Do not imply that commands are implemented unless corresponding code exists.

### Technology Stack

| Component     | Technology                                                      |
| ------------- | --------------------------------------------------------------- |
| Core/CLI      | Rust                                                            |
| CLI parsing   | `clap`                                                          |
| Errors        | `thiserror` for library errors, `anyhow` for CLI boundaries     |
| Serialization | `serde`                                                         |
| Async/runtime | `tokio` when network I/O is needed                              |
| Logging       | `tracing`                                                       |
| Config        | YAML or TOML candidate; see `IMPLEMENTATION.md` before choosing |

### Key Documentation

| Document            | Purpose                                       | Use When                                                               |
| ------------------- | --------------------------------------------- | ---------------------------------------------------------------------- |
| `AGENTS.md`         | Agent workflow and repo conventions           | Before any task in this repo                                           |
| `IMPLEMENTATION.md` | Architecture, tradeoffs, and design decisions | Making implementation, auth, storage, migration, or deployment changes |
| `README.md`         | End-user overview and intended usage          | Updating user-facing behavior or onboarding                            |

## Project Structure

Current scaffold:

```text
lfs-cloud/
  AGENTS.md
  Cargo.lock
  Cargo.toml
  IMPLEMENTATION.md
  README.md
  prettier.config.mjs
  src/
    main.rs
  .editorconfig
  .gitattributes
  .gitignore
  .markdownlint.yml
  .prettierignore
  .vscode/
  .agents/
```

Expected future structure may include:

```text
crates/
  lfs-cloud-cli/
  lfs-cloud-server/
  lfs-cloud-core/
  lfs-cloud-providers/

docs/
  cli/
  config/
  deployment/

tests/
```

Do not create this structure until implementation work needs it.

## Development Guidelines

### Required Context

**Critical before starting any task:** Study `AGENTS.md`, `IMPLEMENTATION.md`, `README.md`.

**Acceptance criteria:**

- Files linted, formatted and saved (see **Lint/Format** below)
- Manual verification steps documented (if `[M]`)

**For Rust code or changes affecting Rust output:**

- Builds clean (`cargo build`)
- All tests pass (`cargo test --all-targets`)
- Doc tests pass (`cargo test --doc`)

**Lint/Format:**

- `cargo fmt` + `cargo clippy` for Rust code.
- `yarn lint:fix` for markdown, shell scripts, other code.
- Only after the above steps, attempt manual formatting/lint-only edits for remaining warnings/errors.

**Package Manager**

Use `cargo` (Cargo.lock present) for Rust code

**Add any learnings to `§ Learnings`** that fit the requirements for that section.

**Update user documentation if the changes affect user-facing features:**

- CLI commands or flags.
- Server configuration.
- Provider support.
- Authentication flows.
- Migration behavior.
- Deployment instructions.
- Security or data-retention expectations.

**Important rules (high‑impact):**

- Stop and report for any blocker that requires human input.
- Consult `IMPLEMENTATION.md` and `README.md` before asking clarification.
- Always write tests for `[T]` tasks and run verification commands after code changes.

### Rust Conventions

| Area           | Convention                                                                  |
| -------------- | --------------------------------------------------------------------------- |
| Error handling | `thiserror` for library errors, `anyhow` for CLI boundaries                 |
| Serialization  | `serde` for config, API payloads, and provider metadata                     |
| Logging        | `tracing` for structured logs                                               |
| CLI            | `clap` for argument parsing                                                 |
| Async          | `tokio` for HTTP and provider I/O                                           |
| Testing        | Unit tests near code, integration tests in `/tests` or crate-level `tests/` |
| Documentation  | Doc comments for public APIs                                                |

### Documentation Standards

**All code must be thoroughly documented**:

````rust
/// A Git LFS object pointer.
///
/// This struct represents the provider-independent metadata stored in a Git LFS
/// pointer file. The same pointer can be migrated between LFS providers as long
/// as the target provider stores the exact same bytes.
///
/// # Examples
///
/// ```
/// let pointer = LfsPointer::new("sha256:abc123", 42);
/// assert_eq!(pointer.size_bytes, 42);
/// ```
pub struct LfsPointer {
    /// SHA-256 object identifier from the Git LFS pointer.
    pub oid: String,

    /// Exact object size in bytes.
    pub size_bytes: u64,
}
````

**Requirements for Rust code:**

- `///` doc comments on all public functions, structs, enums, traits, fields, and variants
- `//!` module-level docs explaining each module's purpose
- `# Examples` sections for non-trivial functions
- `// SAFETY:` comments for every `unsafe` block
- Inline `//` comments for complex algorithms
- Explain **why**, not just **what**

### Test-First Approach

1. Write tests first based on task requirements
2. Tests may be unit or integration depending on task
3. Some tasks require manual verification (marked `[M]` in checklist)
4. Tests can be adapted during implementation if needed

---

## Quick Reference

### Key Libraries

| Purpose | Crate |
| ------- | ----- |

---

## Your Skills

You have access to the following specialized protocols. **Activate** a skill by adopting its persona when the `Trigger` condition is met.

### 🦀 Rust Specialists

| Agent                    | Description                                             | Trigger                                            |
| :----------------------- | :------------------------------------------------------ | :------------------------------------------------- |
| **Rust Core Specialist** | Implementing idiomatic, safe, and performant Rust code. | Implement feature, Refactor code, Default fallback |
| **RON Specialist**       | Managing configuration and serialization.               | Configure settings, Serialize data, .ron files     |
| **Pest Specialist**      | Generating PEG parsers with pest.                       | Define grammar, Parse input, .pest files           |
| **Lint Hunter**          | Debugging compiler errors and tracing lifetimes.        | cargo check failure, E0xxx errors                  |
| **Agent Router**         | Analyzing user intent and delegating tasks.             | New request, Analyze intent                        |

### 🛠️ General Specialists

| Agent                   | Description                           | Trigger                                           |
| :---------------------- | :------------------------------------ | :------------------------------------------------ |
| **Security Specialist** | Auditing for unsafe code and secrets. | Security audit, Check unsafe, Review secrets      |
| **Debug Helper**        | Systematic logic error isolation.     | Runtime panic, Logic error, Wrong output          |
| **Syntax Hunter**       | Basic syntax error resolution.        | Syntax Error, Unexpected token, Missing semicolon |

---

## Learnings

> **Purpose**: Capture critical concise (1-3 lines) insights that are NOT obvious from README.md or IMPLEMENTATION.md, and will be helpful for future development and maintenance.
>
> **Focus**:
>
> - non-obvious gotchas, workarounds, and discoveries
> - repeated failure modes
> - decision-shaping constraints
> - design and structural patterns that encode project conventions, preserve consistency, protect APIs, or prevent recurring bad usage patterns
> - entries that explain why a pattern exists, not just what the code does
>
> **Discard**:
>
> - information already obvious from docs, symbol names, or standard library/framework behavior
> - issues normally surfaced by normal tooling, such as build errors, test failures, or clippy warnings
> - examples as code blocks, link to real code examples in files instead
> - generic best practices unless their use here protects a real repository boundary or recurring failure mode
> - duplicate restatements of the same pattern in multiple areas
> - historical details unless they still affect current design or maintenance decisions
>
> **Entry Format**:
>
> ```
> - **[Area] Topic**: Brief insight (target <= 50 words). See \`SymbolName\` in <path/to/file>.
> ```
>
> Use `Area` as a stable keyword describing the part of the system affected.
> Examples: `build`, `compatibility`, `tests`, `runtime`, `cache`, `providers`, `storage`, `auth`, `migration`, `cli`, `licensing`.
>
> **Example**:
>
> ```
> - **[migration] Pointer stability**: Git LFS pointers contain SHA-256 and size, not provider URLs, so provider migration should copy object bytes and update LFS config rather than rewrite history. See \`LfsPointer\` in crates/lfs-cloud-core/src/pointer.rs.
> ```

- **[build] Reqwest Rustls feature**: Current `reqwest` uses the `rustls` feature name with `default-features = false`; the older `rustls-tls` spelling is invalid for this dependency line. See `reqwest` in Cargo.toml.
- **[errors] Boundary categories**: Cross-domain code should wrap failures in `LfsCloudError` at the boundary that handled them, so `category()` reports the handling area while `source` preserves the underlying provider/storage cause. See `LfsCloudError` in src/error.rs.
- **[logging] Filter composition**: Keep tracing filter construction separate from process-global subscriber installation so server code can reuse or validate filters without consuming the one-shot global subscriber. See `tracing_filter` in src/logging.rs.
- **[providers] Async trait futures**: Public provider traits return explicit `Send` futures instead of using `async fn`, preserving async network backends without adding `async-trait` or triggering public-trait auto-bound ambiguity. See `RepositoryProvider` in src/providers.rs.
- **[compatibility] Git LFS pointer extensions**: Git LFS extension keys are `ext-<single digit>-<name>` where `name` starts with ASCII alphanumeric/underscore and then has no whitespace; values must be `sha256:<oid>`, and arbitrary unknown pointer keys are rejected. See `is_valid_extension_key` in src/lfs.rs.
- **[tests] Integration fixtures**: Shared integration-test helpers live in `tests/support/mod.rs`; each integration test crate imports them with `mod support;` because Rust compiles files under `tests/` as separate crates. See `TempGitRepo` in tests/support/mod.rs.
- **[config] Duplicate provider IDs**: Provider and storage IDs are YAML map keys, so duplicate-key rejection catches repeats during parsing; repository IDs and route paths are validated after typed loading. See `ServerConfig` in src/server_config.rs.
- **[config] Storage credential key**: `credentials_ref` is the documented storage-provider YAML key; `credential_ref` remains a parser alias only for compatibility with older local fixtures. See `RawStorageProviderConfig` in src/server_config.rs.
- **[auth] GitHub OAuth endpoint**: The initial GitHub OAuth browser flow targets GitHub.com explicitly; `api_url` remains the REST API base, not a signal for deriving an enterprise OAuth web host. See `GITHUB_OAUTH_AUTHORIZE_URL` in src/github_auth.rs.
- **[auth] OAuth callback secrecy**: Treat callback `code` and `state` as sensitive at the route boundary; debug output and CSRF mismatch errors must not echo either value. See `GitHubOAuthCallback` in src/github_auth.rs.
- **[auth] OAuth token form encoding**: Keep token exchange form encoding explicit with `url::form_urlencoded`; the current `reqwest` feature set intentionally omits form helpers while preserving Rustls/JSON/stream behavior. See `GitHubOAuthTokenExchanger` in src/github_auth.rs.
- **[auth] OAuth token errors**: Treat non-2xx token responses as auth denials only when JSON or form-encoded bodies contain OAuth diagnostics; generic upstream bodies should stay provider upstream errors. See `parse_token_error_response` in src/github_auth.rs.
- **[auth] GitHub token scopes**: GitHub OAuth requests use space-delimited scopes, but token responses return granted scopes comma-delimited, so keep authorization encoding and response parsing separate. See `parse_scope_list` in src/github_auth.rs.
- **[auth] GitHub API paths**: Append endpoint paths to `api_url` without replacing existing base paths, so GitHub Enterprise-style REST roots such as `/api/v3` remain valid. See `github_user_endpoint` in src/github_auth.rs.
- **[auth] OAuth redaction order**: Redact raw OAuth tokens before truncating GitHub diagnostic messages; exact replacement cannot catch a secret after the diagnostic limit cuts it. See `github_api_error_message` in src/github_auth.rs.
- **[auth] Callback route boundary**: The OAuth callback route may exchange the GitHub code and fetch identity, but must return only non-secret user/scope metadata until session storage issues a separate LFS credential. See `github_oauth_callback_route` in src/github_auth.rs.
- **[auth] Callback response caching**: Apply no-store, no-cache, and no-referrer headers at the OAuth callback router layer so credentials stay out of browser/intermediary caches and even extractor-generated errors inherit the policy. See `protect_github_oauth_callback_response` in src/github_auth.rs.
- **[auth] Callback CSRF registry**: The callback router must consume a registered state per authorization attempt, not store one fixed expected state, so long-lived routers support concurrent logins and reject replay. See `GitHubOAuthStateRegistry` in src/github_auth.rs.
- **[auth] Callback state admission**: Consume digest-keyed callback states before handling provider-denied OAuth errors, expire them at the exact TTL, and reject new login admission at capacity rather than evicting an active attempt. See `GitHubOAuthStateRegistry` in src/github_auth.rs.
- **[auth] OAuth PKCE attempt ownership**: Consume each authorization into the pending-state registry so its S256 verifier has one server-side owner, then remove state and verifier together before exchanging the callback code. See `GitHubOAuthStateRegistry::register` in src/github_auth.rs.
- **[auth] Local LFS sessions**: Git LFS credentials are separate `lfs-cloud` bearer tokens backed by local session metadata; never store or hand GitHub OAuth access tokens to Git credential-helper paths. See `LocalLfsSessionStore` in src/sessions.rs.
- **[auth] Session verification hot path**: Verify only the presented token's expiry and share its OAuth-bearing record through `Arc`; reserve full-store expiry pruning for admission and diagnostics so normal authenticated requests do not scan every session. See `LocalLfsSessionStore::verify_record` in src/sessions.rs.
- **[auth] GitHub permission denial**: Treat GitHub collaborator `404`, `none`, SSO-required, and unknown permission states as authorization denials; do not convert the permission endpoint's `404` into repository-not-found. See `GitHubRepositoryPermissionClient` in src/github_auth.rs.
- **[auth] Git credential path scope**: Persist `credential.<lfs-host>.useHttpPath=true` in the target repository before approving tokens; global or one-shot settings can be overridden locally or disappear before later Git LFS lookups, leaking host-matched credentials across repo paths. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential helper preflight**: Check `git config --get-urlmatch credential.helper <lfs-url>` before `git credential approve`; Git can accept an approve request with no helper configured and then persist nothing. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential URL safety**: Git credential-helper URLs become config keys and protocol input, so reject userinfo and query strings rather than trying to sanitize them later. See `validate_lfs_credential_url` in src/credentials.rs.
- **[auth] Credential lookup scope**: Credential fill must prove protocol, host, path, and username match the configured LFS URL before accepting a stored token, so a host-scoped helper entry cannot satisfy another repo path. See `GitCredentialLookup` in src/credentials.rs.
- **[auth] Credential lookup diagnostics**: Suppress `git credential fill` stderr because a failing helper can echo a stored password before lookup output reveals which value must be redacted. See `GitCredentialLookup::lookup_with_git_program` in src/credentials.rs.
- **[auth] Credential lookup prompts**: Treat credential fill as a non-interactive cache probe; disable terminal, askpass, and GCM interaction together because any remaining prompt path can hang unattended status checks. See `GitCredentialLookup::lookup_with_git_program` in src/credentials.rs.
- **[auth] LFS token transport**: Server auth accepts Bearer tokens and Git LFS Basic credentials where username is `lfs-cloud` and the password is the local session token; route matching still happens before auth so unknown repos remain 404. See `authenticate_lfs_session` in src/server.rs.
- **[auth] Batch authorization token boundary**: Local LFS sessions retain GitHub OAuth tokens only as private server-side state so batch requests can re-check repository permissions while Git LFS receives only the local `lfs-cloud` token. See `LfsSessionRecord` in src/sessions.rs.
- **[storage] Drive credential refs**: Bare Drive `credentials_ref` values map to prefixed env vars containing flat OAuth JSON; use `env:NAME` only when the server operator needs an explicit secret variable. See `GoogleDriveCredentialLoader` in src/google_drive.rs.
- **[storage] Drive token URI safety**: Custom Google OAuth `token_uri` values must use HTTPS and cannot carry query strings or fragments; HTTP is reserved for loopback-only test endpoints. See `validate_token_url` in src/google_drive.rs.
- **[storage] Drive API base safety**: Custom Google Drive API base URLs receive bearer tokens during root validation, so plaintext HTTP is allowed only for loopback test endpoints. See `validate_drive_api_base_url` in src/google_drive.rs.
- **[storage] Drive API path suffix**: Custom Drive API bases may include proxy prefixes, but a path already ending in `/drive/v3` must not append another Drive API segment. See `drive_file_metadata_url` in src/google_drive.rs.
- **[storage] Drive root scope**: The MVP uses `drive.file`; configured root folders must be app-created or explicitly app-accessible, and startup/health checks should validate folder type plus child-write capability before transfers. See `GoogleDriveRootValidator` in src/google_drive.rs.
- **[storage] Drive startup readiness**: Validate every configured Drive credential and writable root before binding the server listener, and reuse the refreshed token cache so readiness does not cause an immediate second OAuth refresh. See `GoogleDriveTransferStore::validate_storage_providers` in src/server.rs.
- **[storage] Drive object lookup**: Drive object paths are for inspection only; lookup must match private app properties for namespace/OID/size and then verify Drive's binary size before accepting the file ID. See `GoogleDriveObjectStore` in src/google_drive.rs.
- **[storage] Drive upload staging**: Verify staged upload file SHA-256 and size before opening a Drive resumable upload session, so bad local temp files cannot create orphaned backend objects. See `GoogleDriveObjectStore::upload_object` in src/google_drive.rs.
- **[storage] Drive session origin**: Drive resumable session `Location` values receive bearer-authenticated upload `PUT`s, so validate them against the configured Drive API origin before forwarding tokens. See `validate_drive_resumable_upload_session_url` in src/google_drive.rs.
- **[storage] Drive upload timeout**: Do not set a reqwest client-level total timeout on the Drive upload client; resumable upload `PUT` streams may legitimately exceed bounded metadata-call deadlines for large LFS objects. See `default_google_drive_object_upload_http_client` in src/google_drive.rs.
- **[storage] Drive download gate**: Drive downloads must resolve file IDs through repository/OID/size app-property lookup before streaming `alt=media`; check media content length before proxying and hash the streamed bytes before completing the response. See `GoogleDriveObjectStore::download_object_response` in src/google_drive.rs.
- **[storage] Drive provider trait**: `GoogleDriveObjectStore` implements the generic storage-provider trait for migration/direct storage flows; server LFS transfers still wrap it separately to record verified-object metadata. See `GoogleDriveObjectStore` in src/google_drive.rs.
- **[metadata] Config-relative DB default**: Omitted `server.metadata_path` resolves to `.lfs-cloud/metadata.sqlite3` beside the config file, keeping server-owned state out of served repositories by default. See `ServerSettings` in src/server_config.rs.
- **[metadata] Connection boundary**: `MetadataDatabase` keeps migrations private and wraps the SQLite connection so future server state can share the handle without exposing migration reruns. See `MetadataDatabase` in src/metadata.rs.
- **[metadata] SQLite table changes**: Schema changes to existing columns need explicit versioned migrations because `CREATE TABLE IF NOT EXISTS` only affects fresh databases. See `NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION` in src/metadata.rs.
- **[metadata] Config sync rows**: Transfer handlers record object rows with foreign keys, so server startup must sync validated config storage/repository mappings into metadata before uploads can upsert verified objects. See `MetadataDatabase::sync_config` in src/metadata.rs.
- **[metadata] Verified object upsert**: Successful uploads upsert one row per repo/storage/OID/size and refresh stale backend IDs in place, so retry and repair paths do not create duplicate object metadata. See `record_verified_object` in src/metadata.rs.
- **[server] Route boundary**: LFS route parsing first proves the request path belongs to a configured repository mapping, then classifies the endpoint suffix; unknown repos stay route denials while unknown suffixes under known repos are malformed requests. See `LfsRouteResolver` in src/server.rs.
- **[server] Batch parse boundary**: Batch JSON parsing happens only after route resolution and local LFS session authentication, so malformed bodies cannot distinguish configured private repos without a valid session. See `handle_lfs_request` in src/server.rs.
- **[server] Download action identity**: Download action URLs include the requested object size because the object route carries only the SHA-256 OID; the GET transfer rebuilds the full `LfsObject` from route OID plus query size before Drive lookup. See `transfer_request_expected_size` in src/server.rs.
- **[server] Upload action identity**: Upload action URLs also include object size; the PUT transfer verifies route OID plus query size against staged bytes before Drive upload and metadata recording. See `handle_lfs_upload_request` in src/server.rs.
- **[server] Error envelope**: LFS route/auth/method/body/parse/authorization failures should return `application/vnd.git-lfs+json` with a `message` field, not plain text, so Git LFS clients see protocol-compatible errors. See `git_lfs_json_error_response` in src/server.rs.
- **[server] Upload staging guardrails**: Keep client-body idle timeouts and temp-directory free-space checks in local staging code, not the Drive upload client, so large resumable backend uploads are not capped by a total request timeout. See `stage_upload_request_body_with_guardrails` in src/server.rs.
- **[server] Provider work amplification**: Count duplicate batch entries toward the request limit but collapse their storage lookups; share one provider semaphore across authorization and storage, and scope short permission reuse to session/repository/operation. See `LfsServerState` in src/server.rs.
- **[server] Batch request admission**: Authenticate before reading batch bodies, then enforce both idle and total read deadlines; cap active HTTP requests process-wide and reject overload rather than queueing slow bodies. See `read_batch_request_body` in src/server.rs.
- **[server] Upload staging admission**: Reserve declared bytes atomically and retain global/per-user staging slots through backend completion; independent filesystem snapshots race across concurrent uploads. See `UploadStagingCoordinator` in src/server.rs.
- **[server] Upload single-flight locks**: Store per-object upload locks weakly and purge dead keys during admission, so retries share a live lock without retaining one allocation per historical OID. See `LfsServerState::upload_lock_for` in src/server.rs.
- **[server] Shutdown drain boundary**: SIGINT and SIGTERM stop listener admission and drain active transfers for 30 seconds; keep the deadline outside Axum's unbounded graceful wait so termination cannot hang indefinitely. See `serve_with_graceful_shutdown` in src/server.rs.
- **[cli] Global config flag**: Keep `--config` as a root `clap` global so future commands share config-path handling while existing `lfs-cloud serve --config ...` usage still parses. See `Cli` in src/cli.rs.
- **[cli] Remote credential safety**: Git remote parsing rejects credentialed HTTPS URLs, plaintext HTTP, query strings, and credential-like scp users before deriving route identity; debug output also redacts raw scp-like userinfo. See `GitRemote` in src/git.rs.
- **[cli] Dot-prefixed repositories**: GitHub repository names may start with a dot, such as `.github`; remote parsing should reject traversal segments without treating a leading repository dot as traversal. See `GitRemote` in src/git.rs.
- **[cli] Init route base URL**: `init --server` normalizes HTTP(S) server bases, rejects trailing slash, unsafe characters, and dot path segments, then appends Git remote route pieces with URL path segments while preserving safe proxy base paths. See `LfsInitRoute` in src/init.rs.
- **[cli] Local config path**: Resolve repository-local Git config paths through `git rev-parse --path-format=absolute --git-path config`; linked worktrees can otherwise make relative `.git` paths ambiguous from the worktree. See `GitRepository::local_git_config_path` in src/git.rs.
- **[cli] Init summary redaction**: `init` may read historical `lfs.url` values from `.lfsconfig` or local Git config; redact userinfo, query strings, and fragments before printing before/after summaries. See `write_init_change` in src/cli.rs.
- **[tests] Git temp paths**: Git may canonicalize macOS temp paths from `/var` to `/private/var`; init tests should compare against canonicalized worktree paths instead of raw `TempDir` paths. See `init_writes_lfsconfig_from_current_repo_origin` in src/cli.rs.
- **[tests] External gates**: Real provider integration tests are ignored by default and guarded by explicit env flags so normal CI compiles them without creating GitHub repos or Drive folders. See `github_disposable_repo_permission_check` in tests/external_integrations.rs.
- **[cli] Login token boundary**: `login` stores only the callback's local `lfs-cloud` token in Git credentials; the GitHub OAuth token remains server-side for permission checks. See `run_login_from_dir` in src/cli.rs.
- **[cli] Status probe boundary**: `status` performs local readiness checks plus TCP reachability and Drive credential parsing; it intentionally does not call GitHub permission APIs or Drive root probes, which remain server/runtime checks. See `run_status_from_dir` in src/cli.rs.
- **[cli] Status URL redaction**: `status` probes the raw `--server` value before route validation, so any reachability output must redact userinfo, query strings, and fragments at the display boundary. See `run_status_from_dir` in src/cli.rs.
- **[cache] Object path identity**: Local cache paths use only the normalized SHA-256 OID under two-level shards; pointer size is verified during ingest/materialization rather than encoded in the filename. See `LocalCacheLayout` in src/local_cache.rs.
- **[cache] Verified ingest**: Repository-local Git LFS objects are stream-hashed while copying into a no-clobber temp-file publication path; existing cache files and publish races are reverified instead of overwritten. See `ingest_git_lfs_object` in src/local_cache.rs.
- **[cache] Worktree registry**: Local cache GC must start from absolute registered worktree roots in `worktrees.json`; relative paths are rejected so future GC does not depend on process cwd. See `LocalCacheWorktreeRegistration` in src/local_cache.rs.
- **[cache] Worktree registry writes**: Register/remove operations hold a separate `worktrees.json.lock` file across load-mutate-save so concurrent `lfs-cloud` processes cannot drop each other's GC roots. See `LocalCacheLayout::lock_worktree_registry` in src/local_cache.rs.
- **[cache] Worktree root identity**: Worktree registration comparisons use canonical path keys when the path exists, so symlinked roots update/remove the same registry row instead of creating duplicate logical worktrees. See `normalized_path_key` in src/local_cache.rs.
- **[cache] Materialization safety**: Cache-to-worktree materialization verifies the cache source first, uses macOS `/bin/cp -c` only as an opportunistic CoW path, and replaces existing worktree files only when they are exact cached bytes or matching LFS pointers. See `materialize_verified_object` in src/local_cache.rs.
- **[cache] Materialization publishing**: Pointer hydration must preserve the replaced worktree file mode and bound pointer reads before parsing, so executable LFS paths stay executable and dirty large files cannot be loaded as pointers. See `read_lfs_pointer_file` in src/local_cache.rs.
- **[cache] Dehydration safety**: Dehydration first preserves verified worktree bytes in the shared cache, then re-verifies the worktree file immediately before pointer publication so dirty edits are rejected rather than replaced. See `LocalCacheLayout::dehydrate_file` in src/local_cache.rs.
- **[cache] CLI dehydration identity**: Path-only `lfs-cloud dehydrate` treats existing valid pointer files as already dehydrated before hashing bytes, because the CLI has no separate expected object identity for distinguishing pointer text payloads from pointer placeholders. See `object_for_dehydration_path` in src/cli.rs.
- **[cache] Local GC reachability**: Local cache GC is pointer-reachability based: it scans registered worktrees for Git LFS pointers and may remove cached bytes for hydrated files that no longer have pointer placeholders, since those bytes can be re-preserved from the worktree. See `LocalCacheLayout::garbage_collect` in src/local_cache.rs.
- **[cache] CLI registry refresh**: CLI `hydrate` and `dehydrate` require current Git worktree registration before cache mutation; `gc` refreshes when run inside a worktree but still runs as cache administration outside one. See `register_current_worktree_for_gc` in src/cli.rs.
- **[cli] Pull media root**: `pull` resolves Git LFS media objects from `lfs.storage` or `git-common-dir` instead of per-worktree `.git`, because linked worktrees/custom LFS storage fetch into shared media dirs. See `git_lfs_objects_dir` in src/cli.rs.
- **[cli] Pull candidate scope**: `pull` filters tracked checkout paths through `git check-attr filter` before pointer parsing, so docs/fixtures pointer text not tracked by LFS are not hydrated. See `current_checkout_lfs_pointer_files` in src/cli.rs.
- **[cli] Pull path streams**: Keep pull path scans as raw NUL-delimited bytes and write `git check-attr` stdin concurrently with stdout collection, so non-UTF-8 paths and large tracked sets do not fail or deadlock. See `current_checkout_lfs_tracked_paths` in src/cli.rs.
- **[build] Rust MSRV**: Keep `Cargo.toml` `rust-version` at least `1.88` while the crate uses Rust 2024 plus let-chain syntax in cache dehydration paths. See `LocalCacheLayout::dehydrate_file` in src/local_cache.rs.
- **[auth] Config debug secrecy**: `ServerConfig` debug output can cross logs and test failures, so provider configs that retain resolved OAuth secrets must redact custom `Debug` fields instead of deriving them. See `GitHubProviderConfig` in src/server_config.rs.
- **[auth] Stable repository identity**: Persist GitHub's numeric repository ID in each mapping and compare it with the current owner/name before permission checks, so rename or name reuse cannot retarget an existing LFS namespace. See `GitHubRepositoryPermissionClient::verify_repository_identity` in src/github_auth.rs.
- **[auth] Stable user identity**: Require GitHub's numeric user ID at login and compare it with the permission response's nested user ID, so a renamed or reused login cannot authorize a different account. See `GitHubRepositoryPermissionClient::check_permission` in src/github_auth.rs.
- **[auth] Protected HTTP transport**: URLs carrying OAuth, LFS credentials, or object bytes require HTTPS or an exact literal loopback IP; trusted-LAN HTTP needs explicit server and client unsafe opt-ins, and token-bearing GitHub clients never follow redirects. See `uses_protected_http_transport` in src/http_transport.rs.
- **[auth] Durable session key**: Production sessions persist only local-token hashes and AEAD-protected GitHub tokens; the key derives from the GitHub OAuth client secret, which must stay stable until active sessions expire or are removed. See `production_session_store` in src/server.rs.
- **[auth] Session revocation ordering**: Logout revokes the server session before rejecting the repository-scoped Git credential; unexpected server failures preserve the credential for retry, while definitive upstream authentication rejection revokes the local session automatically. See `revoke_lfs_session_route` in src/server.rs.
- **[auth] Session admission**: Bound successful issuance by stable provider identity and process-wide capacity; overload must return a retryable error rather than evict another active credential. See `LocalLfsSessionStore` in src/sessions.rs.
- **[migration] Discovery without git-lfs dependency**: Migration discovery reads repo-scoped config and `.gitattributes` directly so planning still reports filters, endpoints, and LFS patterns when the `git lfs` command is absent. See `discover_git_lfs_migration` in src/migration.rs.
- **[migration] Gitattributes comments**: Git treats only line-start `#` as a `.gitattributes` comment; inline `#` tokens are invalid attributes, not comment delimiters, so discovery should not strip them as comments. See `split_gitattributes_line` in src/migration.rs.
- **[migration] Checkout pointer scope**: Current-checkout migration enumeration uses `git check-attr` on tracked paths instead of replaying parsed `.gitattributes`, so Git's own attribute precedence decides which pointer-shaped files count as LFS objects. See `enumerate_current_checkout_lfs_pointers` in src/migration.rs.
- **[migration] History pointer scope**: Selected/all-ref migration enumeration uses `git check-attr --source=<commit>` for each historical tree before parsing pointer blobs, so historical attributes decide LFS ownership without checking out refs. See `enumerate_selected_ref_lfs_pointers` in src/migration.rs.
- **[migration] History scan dedupe**: History scans cache per-commit pointer occurrences and dedupe pointer records by commit/path/object across refs, so aliases and overlapping histories do not inflate migration inventories. See `enumerate_ref_history_lfs_pointers` in src/migration.rs.
- **[migration] History gitlinks**: Historical tree scans must request object type and skip non-blob entries before pointer parsing; gitlinks can match broad LFS attributes but are not blob payloads. See `tree_blobs_at_commit` in src/migration.rs.
- **[migration] Local availability scope**: Migration availability checks should verify both stock Git LFS media storage and the shared cache by SHA-256 plus size before planning source fetches. See `check_local_migration_objects` in src/migration.rs.
- **[migration] Source fetch boundary**: Migration source fetches populate Git LFS media storage with `git lfs fetch` and report post-fetch availability; current/selected-ref fetches clear include/exclude filters, while all-ref fetch uses `--all`. See `fetch_missing_migration_objects` in src/migration.rs.
- **[migration] Source fetch timeout**: `git lfs fetch` may leave `git-lfs` descendants holding stderr open, so timeout cleanup must stop the process tree before joining stderr readers. See `wait_for_git_lfs_fetch_command` in src/migration.rs.
- **[migration] Upload verification**: Migration uploads are idempotent destination writes, but still re-hash local source bytes immediately before provider upload and reject provider-returned object/provider mismatches. See `upload_migration_objects_to_storage` in src/migration.rs.
- **[migration] Purge manifest scope**: The GitHub source-purge helper manifest is intentionally complete for Support, while the main dry-run object listing remains capped for readability; tests should count the two sections separately. See `write_migration_source_purge_report` in src/cli.rs.
- **[tests] Local fake E2E**: The local end-to-end coverage routes fake GitHub/Drive through LFS batch and object transfer handlers before cache hydration, so it verifies server auth/route behavior without real providers or git-lfs. See `local_init_upload_download_and_checkout_flow_uses_fake_providers` in tests/local_end_to_end.rs.
- **[tests] Shared integration support**: Files under `tests/` compile as separate crates, so reusable `tests/support` helpers may be intentionally unused per crate and should suppress dead-code warnings at the support-module boundary. See `tests/support/mod.rs`.
- **[auth] Credential helper process groups**: Credential Git commands need an owned process group and joined pipe readers; a successful direct child does not imply helper descendants released inherited stdout/stderr. See `git_command` in src/credentials.rs.
