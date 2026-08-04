# AGENTS.md

This file provides context for AI agents working in this codebase.

## Project Overview

**LFS Cloud** is a Git LFS-compatible server and CLI for moving Git LFS object storage away from the Git host while keeping normal Git repository hosting unchanged.

The initially supported shape is:

- Git repositories remain on GitHub.
- Git LFS clients point at an LFS Cloud endpoint through `.lfsconfig` or local Git config.
- LFS Cloud authorizes users through the repository provider's permissions.
- Actual large-file bytes are stored in Google Drive.
- The local CLI reduces disk duplication by using a shared content-addressed cache and copy-on-write materialization where supported.

Future repository providers may include GitLab, Bitbucket, and self-hosted Git services. Future storage providers may include local filesystem, S3-compatible storage, R2, B2, and MinIO. Keep the abstraction boundaries in place, but do not present future providers as initially supported.

This repository is in pre-release development. Do not imply that a command, provider, or release package exists unless corresponding code or automation exists.

### Technology Stack

| Component     | Technology                                                  |
| ------------- | ----------------------------------------------------------- |
| Core/CLI      | Rust                                                        |
| CLI parsing   | `clap`                                                      |
| Errors        | `thiserror` for library errors, `anyhow` for CLI boundaries |
| Serialization | `serde`                                                     |
| Async/runtime | `tokio` when network I/O is needed                          |
| Logging       | `tracing`                                                   |
| Config        | YAML; see `docs/configuration.md`                           |

### Key Documentation

| Document | Purpose | Use When |
| --- | --- | --- |
| `AGENTS.md` | Agent workflow and repo conventions | Before any task in this repo |
| `README.md` | Current end-user overview | Updating user-facing behavior or onboarding |
| `docs/configuration.md` | Server configuration reference | Changing config, auth, storage, or routing |
| `docs/install-release.md` | Build, install, and release shape | Changing packaging, CI, or distribution |
| `docs/history/implementation.md` | Historical architecture/design record | Researching earlier decisions |
| `docs/history/findings.md` | Historical implementation review findings | Researching completed review work |

## Project Structure

Current high-level structure:

```text
lfscloud/
  AGENTS.md
  Cargo.lock
  Cargo.toml
  README.md
  docs/
    configuration.md
    install-release.md
    history/
      findings.md
      implementation.md
      pre-release-readme.md
  src/
  tests/
  .agents/
```

The implementation remains a single root Rust package. Do not split it into a workspace until concrete module boundaries justify that change.

## Development Guidelines

### Required Context

**Critical before starting any task:** Study `AGENTS.md`, `README.md`, and the task-relevant current guide under `docs/`. Consult historical documents only when earlier design context is relevant.

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

Use `cargo` (Cargo.lock present) for Rust code.

**Add any learnings to `§ Learnings`** that fit the requirements for that section. **Update or remove any stale entries.**

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
- Consult `README.md` and the task-relevant current guide under `docs/` before asking clarification.
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
- No hardcoded secrets, use `std::env::var`
- Use `dbg!()` macros to debug (better than `println!`), remove before final commit

### Test-First Approach

1. Write tests first based on task requirements
2. Tests may be unit or integration depending on task
3. Some tasks require manual verification (marked `[M]` in checklist)
4. Tests can be adapted during implementation if needed

---

## Learnings

> **Purpose**: Capture critical concise (1-3 lines) insights that are NOT obvious from current user documentation or the historical implementation notes, and will be helpful for future development and maintenance.
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
> Use `Area` as a stable keyword describing the part of the system affected. Examples: `build`, `compatibility`, `tests`, `runtime`, `cache`, `providers`, `storage`, `auth`, `migration`, `cli`, `licensing`.
>
> **Example**:
>
> ```
> - **[migration] Pointer stability**: Git LFS pointers contain SHA-256 and size, not provider URLs, so provider migration should copy object bytes and update LFS config rather than rewrite history. See \`LfsPointer\` in crates/lfscloud-core/src/pointer.rs.
> ```

- **[build] Reqwest Rustls feature**: Current `reqwest` uses the `rustls` feature name with `default-features = false`; the older `rustls-tls` spelling is invalid for this dependency line. See `reqwest` in Cargo.toml.
- **[build] Target-native lint coverage**: Run Clippy on every release target because cfg-gated Windows code and test imports are invisible to Unix development hosts. See `build-test-smoke` in .github/workflows/ci.yml.
- **[build] Windows release completion**: Keep the Windows status pending until the existing release contains digest-verified archive, checksum, and manifest assets; this preserves safe retries after interrupted publication. See `Publish-WindowsReleaseAssets` in scripts/release.ps1.
- **[build] PowerShell native argument arrays**: Flatten repeated native-command arguments before binding them through a `[string[]]` function parameter; nested arrays are coerced to one space-joined string. See `Get-WindowsReleaseUploadArguments` in scripts/release.ps1.
- **[build] Changelog release notes**: Version commits roll `Unreleased` into dated sections, but drafts consume nothing; notes include every section newer than the highest published stable release until publication advances the cutoff. See `release_extract_cumulative_changelog_notes` in scripts/lib/release-common.sh.
- **[build] Release increment idempotency**: Repeated version commands resume an untagged matching `Release vX.Y.Z` commit and reject an already-tagged `HEAD`; a new commit is required before another increment. See `release_classify_version_action` in scripts/lib/release-common.sh.
- **[build] In-container release root**: The inner Linux verifier skips `release_initialize` because the outer wrapper owns GitHub status, so bind `RELEASE_REPO_ROOT` to `/workspace` before calling shared artifact-path helpers. See scripts/docker/run-linux-verification.sh.
- **[build] Capability-based local verification**: Select the native verifier and Docker verifiers independently; a missing Docker engine must not suppress a runnable macOS or Windows check. See `verify_all_configure_default_commands` in scripts/local/verify-all.sh.
- **[build] Cross-machine release verification**: Create the tagged asset-less draft before the version verifier wave so Windows can continue concurrently; keep child output in retained per-environment logs and attach macOS/Linux assets only after their statuses pass. See `verify_all_run_parallel` in scripts/local/verify-all.sh and `prepare_release_draft` in scripts/release.sh.
- **[build] Fleet release resumption**: Fast-forward only a clean Windows `main` on `E:\`, target Windows and publication by exact tag, and skip ordinary base checks only when `HEAD` is the matching tagged `Release vX.Y.Z` commit. A subject match without that tag must repair through the checked path. See `release_all_prepare_candidate` in scripts/release-all.sh.
- **[build] Bash 3.2 empty arrays**: With `set -u`, forward possibly empty arrays through the `[@]+` expansion so no-work verification waves receive zero arguments instead of failing or receiving one empty argument. See `release_all_ensure_base_verifications` in scripts/release-all.sh.
- **[build] Windows SSH GitHub auth**: Key-authenticated Windows OpenSSH sessions cannot use the desktop credential store, so send the validated Mac `gh` token through SSH stdin, validate only the active `gh` account, and expose it as a transient `GH_TOKEN`; keep repository-sync jobs token-free. See `release_all_windows_execute_authenticated_script` in scripts/release-all.sh.
- **[build] Windows SSH Rust proxies**: Key-authenticated Windows OpenSSH can reject rustup-created executable symlinks, and PowerShell 7.6 can reject native file redirection. Use `rustup run`, real `RUSTC`/`RUSTDOC` paths, and a regular Cargo proxy copy for nested Node/Bash smoke commands. See `New-ReleaseRustupProxyShim` in scripts/lib/release-common.ps1.
- **[build] PowerShell conditional arrays**: PowerShell enumerates arrays emitted from `if` expressions; type and return single-selection index arrays as one object so StrictMode `.Count` checks remain valid. See `Get-WindowsReleaseSelectionIndices` in scripts/release.ps1.
- **[compatibility] Windows Git paths**: Normalize existing absolute paths reported by Git with `dunce::canonicalize`; Git uses slash-separated drive paths while `std::fs::canonicalize` adds verbatim prefixes, and mixing them breaks containment, output, and equality checks. See `detect_worktree_root` in src/git.rs.
- **[errors] Boundary categories**: Cross-domain code should wrap failures in `LfsCloudError` at the boundary that handled them, so `category()` reports the handling area while `source` preserves the underlying provider/storage cause. See `LfsCloudError` in src/error.rs.
- **[logging] Filter composition**: Keep tracing filter construction separate from process-global subscriber installation so server code can reuse or validate filters without consuming the one-shot global subscriber. See `tracing_filter` in src/logging.rs.
- **[licensing] Release metadata**: Project source is MIT-licensed; keep the root license text and both package metadata files aligned, and recheck locked dependency license expressions before distributing artifacts. See `license` in Cargo.toml.
- **[providers] Async trait futures**: Public provider traits return explicit `Send` futures instead of using `async fn`, preserving async network backends without adding `async-trait` or triggering public-trait auto-bound ambiguity. See `RepositoryProvider` in src/providers.rs.
- **[providers] Repository auth context**: Production batch authorization must dispatch through `RepositoryProvider` with the redacted per-session actor/token context; direct concrete-client calls bypass the extensibility contract and let test adapters model a different boundary. See `RepositoryAuthentication` in src/providers.rs.
- **[tests] Provider contract parity**: Run repository permission and stable-identity isolation assertions against both production adapters and integration fakes; fake-only trait tests can encode a different permission lattice while still passing. See `assert_repository_permission_contract` in tests/support/mod.rs.
- **[tests] Storage contract parity**: Run the shared lifecycle contract against fakes, direct adapters, and configured provider wrappers. The fixture is also `include!`d by unit tests, so keep its parent imports compatible and package `tests/` with crate sources. See `assert_storage_provider_contract` in tests/support/storage_provider_contract.rs.
- **[tests] Live provider composition**: Seed a durable local session, then launch `CARGO_BIN_EXE_lfscloud` and drive it with real Git LFS so gated tests cover process wiring, GitHub authorization, Drive transfers, SQLite recording, and checkout bytes without browser automation. See `black_box_git_lfs_push_fetch_uses_live_github_and_drive` in tests/external_integrations.rs.
- **[tests] Live migration source isolation**: Keep live migration Git refs on an SSH-shaped remote rewritten to a local bare repository and serve legacy bytes from loopback HTTP, so source setup needs no GitHub LFS scope while destination writes still cross the compiled server, GitHub permission check, and Drive. See `git_lfs_historical_migration_round_trip` in tests/external_integrations.rs.
- **[tests] Prerequisite gates**: Keep external-service tests ignored by default, but once a developer explicitly selects one, missing enable flags, credentials, Git, or Git LFS must fail clearly instead of returning a successful no-op. See `require_enabled` in tests/external_integrations.rs.
- **[tests] Credential helper stdin fixtures**: Fake `git credential approve` helpers must drain stdin before exiting; otherwise a fast helper can race the parent write and replace the intended assertion path with `BrokenPipe`. See `approve_failure_normalizes_multiline_command_stderr` in src/credentials.rs.
- **[tests] Smoke server readiness**: Background smoke servers need a liveness-checked startup handshake with enough time for loaded CI runners; short fixed port-file polling windows create target-specific flakes. See `verify-logout-command.sh` in scripts/manual.
- **[tests] Windows upload fixtures**: Axum upload fixtures must consume request bodies before responding; otherwise Windows can abort the connection before the intended simulated response reaches the client. See `failed_server_upload_leaves_both_target_config_locations_unchanged` in src/cli/migration.rs.
- **[compatibility] Git LFS pointer extensions**: Git LFS extension keys are `ext-<single digit>-<name>` where `name` starts with ASCII alphanumeric/underscore and then has no whitespace; values must be `sha256:<oid>`, and arbitrary unknown pointer keys are rejected. See `is_valid_extension_key` in src/lfs.rs.
- **[compatibility] Git LFS batch identities**: Batch `hash_algo` may be omitted only because the protocol defaults it to `sha256`; batch OIDs are raw 64-character lowercase hex, unlike prefixed pointer OIDs or the tolerant programmatic constructor. See `LfsBatchHashAlgorithm` in src/lfs.rs.
- **[compatibility] Empty Git LFS pointers**: A zero-byte file is its own canonical pointer; parse it as the empty-content SHA-256, render zero-size pointers as zero bytes, and exclude them from cache and migration transfer inventories. See `LfsPointer::is_empty` in src/lfs.rs.
- **[compatibility] Git LFS pointer size**: Pointer files must be strictly smaller than Git LFS's 1,024-byte cutoff; share the protocol constant across direct parsing, checkout/cache reads, and migration blob discovery. See `LFS_POINTER_SIZE_CUTOFF` in src/lfs.rs.
- **[compatibility] Git LFS pointer canonicalization**: Reject uppercase pointer OIDs, but accept the reference decoder's non-canonical blank lines, CRLF endings, and missing final newline; parsing recognizes compatible history while rendering rewrites canonical bytes. See `LfsPointer::parse` in src/lfs.rs.
- **[compatibility] Git LFS pointer versions**: Accept the reference client's alpha and pre-release v1 URL aliases when reading historical pointers, but always render the current public v1 URL. See `LFS_POINTER_VERSION_ALIASES` in src/lfs.rs.
- **[compatibility] Git LFS extension priorities**: Extension names may differ, but their single-digit execution priorities must be unique across a pointer; enforce this during both parsing and programmatic construction. See `LfsPointer::insert_extension` in src/lfs.rs.
- **[tests] Integration fixtures**: Shared integration-test helpers live in `tests/support/mod.rs`; each integration test crate imports them with `mod support;` because Rust compiles files under `tests/` as separate crates. See `TempGitRepo` in tests/support/mod.rs.
- **[tests] Qualified test selectors**: Manual verifiers and self-spawning tests must update fully qualified filters after module moves and validate them against `cargo test -- --list`; an unmatched Cargo filter can exit successfully after running zero tests. See `redaction_tests` in scripts/manual/verify-secret-redaction.sh.
- **[tests] Default-config binary smoke**: Resolve or build the exact binary before overriding or unsetting `HOME`; a Cargo fallback depends on the same home environment and would test the harness instead of installed CLI behavior. See scripts/manual/verify-default-config-path.sh.
- **[tests] Managed-default smoke boundary**: Exact-binary smoke fixtures must omit defaulted network and GitHub API fields to prove installed behavior; keep native credential-store mutation out of ordinary runs, and cover safe rotation failures without reaching Keychain, Credential Manager, or Secret Service. See `defaultServerStartupSmoke` in .agents/skills/smoke-test/scripts/smoke-test.ts.
- **[config] Duplicate provider IDs**: Provider and storage IDs are YAML map keys, so duplicate-key rejection catches repeats during parsing; repository IDs and route paths are validated after typed loading. See `ServerConfig` in src/server_config/config.rs.
- **[config] Installed config default**: Omitted `--config` resolves `lfscloud.yml` below `HOME`, with `USERPROFILE` as the Windows fallback; never make installed CLI behavior depend on the process working directory. See `ServerConfig::default_path` in src/server_config/config.rs.
- **[runtime] Per-connection action origin**: A wildcard listener derives direct Git LFS action origins from the accepted socket's local destination, not untrusted host headers; embedded routers without `public_url` must use the exported connect-info adapter. See `AcceptedSocketAddress` in src/server/runtime.rs.
- **[auth] Managed session key identity**: Namespace native credential-store keys by a stable metadata installation ID, and never recreate a missing key while active encrypted rows exist; confirmed rotation holds the server lifecycle lock and invalidates rows first. See `load_or_create_managed_session_key` in src/session_keys.rs.
- **[auth] GitHub API paths**: Append endpoint paths to `api_url` without replacing existing base paths, so GitHub Enterprise-style REST roots such as `/api/v3` remain valid. See `github_user_endpoint` in src/github_auth.rs.
- **[auth] GitHub PAT boundary**: Verify each caller's PAT against GitHub only to establish identity, then issue a local token and recheck that retained PAT against the mapped repository for every LFS operation. See `GitHubPersonalAccessTokenLoginRouteState` in src/github_auth.rs.
- **[auth] Local LFS sessions**: Git LFS credentials are separate LFS Cloud bearer tokens backed by local session metadata; never store or hand a user's GitHub PAT to Git credential-helper paths. See `LocalLfsSessionStore` in src/sessions.rs.
- **[auth] Session verification hot path**: Verify only the presented token's expiry and share its PAT-bearing record through `Arc`; reserve full-store expiry pruning for admission and diagnostics so normal authenticated requests do not scan every session. See `LocalLfsSessionStore::verify_record` in src/sessions.rs.
- **[auth] Session-key migrations**: When an authentication mode changes the durable-session root secret, invalidate older protected rows in a schema migration before loading sessions so undecryptable legacy credentials cannot block startup. See `PAT_AUTHENTICATION_SESSION_MIGRATION` in src/metadata/migrations.rs.
- **[auth] GitHub permission denial**: Treat GitHub collaborator `404`, `none`, SSO-required, and unknown permission states as authorization denials; do not convert the permission endpoint's `404` into repository-not-found. See `GitHubRepositoryPermissionClient` in src/github_auth.rs.
- **[auth] Git credential path scope**: Persist `credential.<lfs-host>.useHttpPath=true` in the target repository before approving tokens; global or one-shot settings can be overridden locally or disappear before later Git LFS lookups, leaking host-matched credentials across repo paths. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential helper preflight**: Check `git config --get-urlmatch credential.helper <lfs-url>` before `git credential approve`; Git can accept an approve request with no helper configured and then persist nothing. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential URL safety**: Git credential-helper URLs become config keys and protocol input, so reject userinfo and query strings rather than trying to sanitize them later. See `validate_lfs_credential_url` in src/credentials.rs.
- **[auth] Credential lookup scope**: Credential fill must prove protocol, host, path, and username match the configured LFS URL before accepting a stored token, so a host-scoped helper entry cannot satisfy another repo path. See `GitCredentialLookup` in src/credentials.rs.
- **[auth] Credential lookup diagnostics**: Suppress `git credential fill` stderr because a failing helper can echo a stored password before lookup output reveals which value must be redacted. See `GitCredentialLookup::lookup_with_git_program` in src/credentials.rs.
- **[auth] Credential lookup prompts**: Treat credential fill as a non-interactive cache probe; disable terminal, askpass, and GCM interaction together because any remaining prompt path can hang unattended status checks. See `GitCredentialLookup::lookup_with_git_program` in src/credentials.rs.
- **[auth] LFS token transport**: Server auth accepts Bearer tokens and Git LFS Basic credentials where username is `lfscloud` and the password is the local session token; route matching still happens before auth so unknown repos remain 404. See `authenticate_lfs_session` in src/server.rs.
- **[auth] Batch authorization token boundary**: Local LFS sessions retain each user's GitHub PAT only as private server-side state so batch requests can re-check repository permissions while Git LFS receives only the local LFS Cloud token. See `LfsSessionRecord` in src/sessions.rs.
- **[auth] Login identity boundary**: GitHub PAT login verifies the presented user's identity but grants no repository ACL; each LFS operation rechecks that user's current GitHub permission, with reads authorizing fetch and writes authorizing upload or migration. See `github_personal_access_token_login_route` in src/github_auth.rs.
- **[migration] Server transfer boundary**: Migration reconciles OIDs through the authenticated upload batch before fetching bytes and follows only same-origin server upload actions; clients never construct storage providers or access Drive. See `request_migration_target_plan` in src/cli/migration.rs.
- **[migration] Committed legacy source**: Preserve the prior endpoint as `.lfsconfig` `remote.<name>.lfsurl`; repository-wide `lfs.url` remains active normally, while follow-up migration applies the legacy endpoint only as a command-scoped source fetch override. See `write_worktree_remote_lfs_url` in src/git.rs.
- **[storage] gcloud ADC isolation**: Use isolated `CLOUDSDK_CONFIG` state and a project-specific Desktop OAuth `--client-id-file` whose project enables Drive; shared-client tokens can fail quota/API checks. Explicit scopes need both `cloud-platform` and `drive.file`. See `GoogleDriveGcloudTokenProvider` in src/google_drive.rs.
- **[compatibility] Windows gcloud launcher**: Default to `gcloud.cmd` on Windows because a bare `gcloud` process name does not resolve the standard Cloud SDK command launcher there; Node smoke probes must invoke the batch launcher through `cmd.exe`. See `DEFAULT_GCLOUD_EXECUTABLE` in src/server_config/providers.rs and `gcloudInvocation` in .agents/skills/smoke-test/scripts/smoke-test.ts.
- **[storage] Drive API base safety**: Custom Google Drive API base URLs receive bearer tokens during root validation, so plaintext HTTP is allowed only for literal loopback IP test endpoints, not hostnames such as `localhost`. See `validate_drive_api_base_url` in src/google_drive.rs.
- **[storage] Drive API path suffix**: Custom Drive API bases may include proxy prefixes, but a path already ending in `/drive/v3` must not append another Drive API segment. See `drive_api_url` in src/google_drive.rs.
- **[storage] Drive root scope**: The MVP uses `drive.file`; configured root folders must be app-created or explicitly app-accessible, and startup/health checks should validate folder type plus child-write capability before transfers. See `GoogleDriveRootValidator` in src/google_drive.rs.
- **[storage] Drive startup readiness**: Validate every configured gcloud ADC source and writable root before binding the server listener, and share the minted token cache with repository-scoped adapters so readiness does not cause an immediate second CLI invocation. See `ServerStorageProviderFactory::build` in src/provider_factory.rs.
- **[storage] Drive object lookup**: Drive object paths are for inspection only; lookup must match private app properties for namespace/OID/size and then verify Drive's binary size before accepting the file ID. See `GoogleDriveObjectStore` in src/google_drive.rs.
- **[storage] Drive indexed sharding**: Generic server lookups use Drive's optional backend-ID capability for direct `files.get`; stale IDs fall back to root/shard discovery and conditional metadata repair. New uploads use deterministic first-byte shards while legacy root objects remain discoverable. See `StorageProviderTransferStore::lookup_and_repair_object` in src/server.rs.
- **[storage] Drive lookup pagination**: A Drive `nextPageToken` is not duplicate evidence; validate every page before deciding absence or selecting the smallest exact-match file ID, and reject repeated tokens so malformed pagination cannot loop forever. See `GoogleDriveObjectStore::lookup_object` in src/google_drive.rs.
- **[storage] Drive namespace properties**: Preserve raw repository namespaces only while the app-property key/value fits Drive's 124-byte UTF-8 limit; oversized values use a SHA-256 digest plus a format marker to retain compatibility without raw/digest ambiguity. See `GoogleDriveRepositoryNamespaceProperty` in src/google_drive.rs.
- **[storage] Drive upload staging**: Verify staged upload file SHA-256 and size before opening a Drive resumable upload session, so bad local temp files cannot create orphaned backend objects. See `GoogleDriveObjectStore::upload_object` in src/google_drive.rs.
- **[storage] Drive session origin**: Drive resumable session `Location` values receive bearer-authenticated upload `PUT`s, so validate them against the configured Drive API origin before forwarding tokens. See `validate_drive_resumable_upload_session_url` in src/google_drive.rs.
- **[storage] Drive transfer timeouts**: Bound connects and reset an idle watchdog on transfer progress without a total deadline; reqwest's client read timeout starts before upload response headers, so uploads need a body-aware watchdog instead. See `send_drive_upload_with_idle_timeout` in src/google_drive.rs.
- **[storage] Drive error reasons**: Classify documented Drive reason codes before HTTP status fallbacks; many quota, permission, and rate-limit failures all arrive as HTTP 403 but require different retry and operator actions. See `classify_common_drive_error` in src/google_drive.rs.
- **[storage] Drive resumable recovery**: Send staged files in 256 KiB-aligned chunks and treat Drive's `Range` response as the committed-offset authority; after retryable interruption, probe the same session with bounded backoff before the outer idempotent path creates another file. See `recover_drive_resumable_upload` in src/google_drive.rs.
- **[storage] Drive upload idempotency**: Writers sharing a Drive root must share metadata locks across lookup/upload. Verify staged bytes before waiting, but mint or refresh the Drive token after lock acquisition; exact historical duplicates resolve to the smallest validated file ID. See `GoogleDriveStorageProvider::upload_object` in src/provider_factory.rs.
- **[providers] Config registration boundary**: Concrete config variants register runtime adapters, readiness, metadata descriptors, and optional server capabilities in one factory module; production callers stay on provider-neutral registries. See `StorageProviderRegistration` in src/provider_factory.rs.
- **[storage] Drive download gate**: Drive downloads must resolve file IDs through repository/OID/size app-property lookup before streaming `alt=media`; check media content length before proxying and hash the streamed bytes before completing the response. See `GoogleDriveObjectStore::download_object_response` in src/google_drive.rs.
- **[storage] Drive provider layers**: `GoogleDriveStorageProvider` supplies the shared contract, while the server upload handler owns the durable lock across lookup and upload and composes generic metadata repair plus optional capabilities; migration reaches only that server action route. See `handle_lfs_upload_request` in src/server.rs.
- **[storage] Fallback download reconciliation**: Providers without streaming support must repair or stale durable object metadata from the staged download result; preflight lookup would duplicate provider discovery. See `staged_download_response` in src/server.rs.
- **[metadata] Config-relative DB default**: Omitted `server.metadata_path` resolves to `.lfscloud/metadata.sqlite3` beside the config file, keeping server-owned state out of served repositories by default. See `ServerSettings` in src/server_config/settings.rs.
- **[metadata] Connection boundary**: `MetadataDatabase` keeps migrations private and wraps the SQLite connection so future server state can share the handle without exposing migration reruns. See `MetadataDatabase` in src/metadata/mod.rs.
- **[tests] SQLite lock timing**: Async metadata lock tests prove runtime responsiveness separately from post-release completion; allow the latter at least the configured SQLite busy timeout because a blocked connection may not observe release immediately on loaded Windows runners. See `async_verified_object_write_does_not_block_runtime_worker` in src/metadata/objects.rs.
- **[metadata] SQLite table changes**: Schema changes to existing columns need explicit versioned migrations because `CREATE TABLE IF NOT EXISTS` only affects fresh databases. See `NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION` in src/metadata/migrations.rs.
- **[metadata] Forward schema guard**: Read and reject a `user_version` newer than `METADATA_SCHEMA_VERSION` before executing any schema SQL, so an older binary cannot mutate an unknown future database. See `MetadataDatabase::run_migrations` in src/metadata/migrations.rs.
- **[metadata] Config sync rows**: Transfer handlers record object rows with foreign keys, so server startup must sync validated config storage/repository mappings into metadata before uploads can upsert verified objects. See `MetadataDatabase::sync_config` in src/metadata/configuration.rs.
- **[metadata] Config route reconciliation**: Config sync tombstones routes for removed or renamed mappings and marks them inactive before current upserts, preserving object/attempt history while releasing unique public routes. See `release_inactive_repository_routes` in src/metadata/configuration.rs.
- **[metadata] Verified object upsert**: Successful uploads upsert one row per repo/storage/OID/size and refresh stale backend IDs in place while preserving the first creator's attribution, so retry and repair paths neither create duplicates nor rewrite provenance. See `record_verified_object` in src/metadata/objects.rs.
- **[metadata] Transfer attempt lifecycle**: Start durable attempt rows only after route, session, size, and permission validation; complete them with backend IDs on success or fixed secret-free diagnostics on failure, leaving `started` only for interrupted work. See `MetadataDatabase::start_transfer_attempt` in src/metadata/transfers.rs.
- **[server] Route boundary**: LFS route parsing first proves the request path belongs to a configured repository mapping, then classifies the endpoint suffix; unknown repos stay route denials while unknown suffixes under known repos are malformed requests. See `LfsRouteResolver` in src/server.rs.
- **[server] Batch parse boundary**: Batch JSON parsing happens only after route resolution and local LFS session authentication, so malformed bodies cannot distinguish configured private repos without a valid session. See `handle_lfs_request` in src/server.rs.
- **[server] Download action identity**: Download action URLs include the requested object size because the object route carries only the SHA-256 OID; the GET transfer rebuilds the full `LfsObject` from route OID plus query size before Drive lookup. See `transfer_request_expected_size` in src/server.rs.
- **[server] Upload action identity**: Upload action URLs also include object size; the PUT transfer verifies route OID plus query size against staged bytes before Drive upload and metadata recording. See `handle_lfs_upload_request` in src/server.rs.
- **[server] Error envelope**: LFS route/auth/method/body/parse/authorization failures should return `application/vnd.git-lfs+json` with a `message` field, not plain text, so Git LFS clients see protocol-compatible errors. See `git_lfs_json_error_response` in src/server.rs.
- **[server] Upload staging guardrails**: Keep client-body idle timeouts and temp-directory free-space checks in local staging code, not the Drive upload client, so large resumable backend uploads are not capped by a total request timeout. See `stage_upload_request_body_with_guardrails` in src/server.rs.
- **[server] Provider work amplification**: Count duplicate batch entries toward the request limit but collapse their storage lookups; share one provider semaphore across authorization and storage, and scope short permission reuse to session/repository/operation. See `LfsServerState` in src/server.rs.
- **[server] Router composition**: Keep `lfs_server_router` as the public zero-setup embedder entry point; internal composition belongs in `LfsRouterBuilder`, with one request-limit layer around either the standalone LFS router or the complete auth/session/LFS merge. See `LfsRouterBuilder` in src/server.rs.
- **[server] Batch request admission**: Authenticate before reading batch bodies, then enforce both idle and total read deadlines; cap active HTTP requests process-wide and reject overload rather than queueing slow bodies. See `read_batch_request_body` in src/server.rs.
- **[server] Upload staging admission**: Reserve declared bytes atomically and retain global/per-user staging slots through backend completion; independent filesystem snapshots race across concurrent uploads. See `UploadStagingCoordinator` in src/server.rs.
- **[server] Upload single-flight locks**: Store per-object upload locks weakly and purge dead keys during admission, so retries share a live lock without retaining one allocation per historical OID. See `LfsServerState::upload_lock_for` in src/server.rs.
- **[server] Shutdown drain boundary**: SIGINT and SIGTERM stop listener admission and drain active transfers for 30 seconds; keep the deadline outside Axum's unbounded graceful wait so termination cannot hang indefinitely. See `serve_with_graceful_shutdown` in src/server.rs.
- **[cli] Global config flag**: Keep `--config` as a root `clap` global so future commands share config-path handling while existing `lfscloud serve --config ...` usage still parses. See `Cli` in src/cli.rs.
- **[cli] Remote credential safety**: Git remote parsing rejects credentialed HTTPS URLs, plaintext HTTP, query strings, and credential-like scp users before deriving route identity; debug output also redacts raw scp-like userinfo. See `GitRemote` in src/git.rs.
- **[cli] Dot-prefixed repositories**: GitHub repository names may start with a dot, such as `.github`; remote parsing should reject traversal segments without treating a leading repository dot as traversal. See `GitRemote` in src/git.rs.
- **[cli] Init route base URL**: `init --server` normalizes HTTP(S) server bases, rejects trailing slash, unsafe characters, and dot path segments, then appends Git remote route pieces with URL path segments while preserving safe proxy base paths. See `LfsInitRoute` in src/init.rs.
- **[cli] Local config path**: Resolve repository-local Git config paths through `git rev-parse --path-format=absolute --git-path config`; linked worktrees can otherwise make relative `.git` paths ambiguous from the worktree. See `GitRepository::local_git_config_path` in src/git.rs.
- **[cli] Init summary redaction**: `init` may read historical `lfs.url` values from `.lfsconfig` or local Git config; redact userinfo, query strings, and fragments before printing before/after summaries. See `write_init_change` in src/cli.rs.
- **[tests] Git temp paths**: Git may canonicalize macOS temp paths from `/var` to `/private/var`; init tests should compare against canonicalized worktree paths instead of raw `TempDir` paths. See `init_writes_lfsconfig_from_current_repo_origin` in src/cli.rs.
- **[tests] External gates**: Real provider integration tests are ignored by default and guarded by explicit env flags so normal CI compiles them without creating GitHub repos or Drive folders. See `github_disposable_repo_permission_check` in tests/external_integrations.rs.
- **[tests] Process-tree portability**: Route bounded Git/Git LFS waits through `wait_for_child`, then re-execute the native Rust test binary for timeout descendants so Unix process groups and Windows recursive `taskkill` cleanup share one regression contract. See `wait_for_child` in src/child_process.rs and `command_timeout_stops_descendant_helpers` in src/credentials.rs.
- **[tests] Windows Git smoke paths**: Git Bash evaluates credential-helper values as shell snippets while CLI output retains native separators; use slash-normalized helper paths and normalize captured output only for assertions. See `IsolatedGit::initialize` in tests/external_integrations.rs.
- **[tests] Smoke Git config isolation**: Pass the sandbox `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_NOSYSTEM` environment to both fixture setup and tested binaries that invoke Git; mixing isolated setup with ambient child Git makes repository discovery depend on runner configuration. See `isolatedGitEnv` in .agents/skills/smoke-test/scripts/smoke-test.ts.
- **[cli] Login token boundary**: `login` submits the PAT to the protected server route but stores only the returned local LFS Cloud token in Git credentials; the PAT remains private server-side for permission checks. See `run_login_from_dir` in src/cli.rs.
- **[cli] Login token input**: Read interactive PATs with terminal echo disabled, but keep piped automation input on stdin; both paths must share the same byte-bounded line reader so hiding the terminal does not reintroduce unbounded allocation. See `read_hidden_login_token` in src/cli.rs.
- **[cli] Status probe boundary**: `status` performs local readiness checks plus TCP reachability and Drive credential parsing; it intentionally does not call GitHub permission APIs or Drive root probes, which remain server/runtime checks. See `run_status_from_dir` in src/cli.rs.
- **[cli] Status URL redaction**: `status` probes the raw `--server` value before route validation, so any reachability output must redact userinfo, query strings, and fragments at the display boundary. See `run_status_from_dir` in src/cli.rs.
- **[cli] URL redaction composition**: Unvalidated display input can match URL and scp-like shapes simultaneously; redact query/fragment suffixes before choosing a userinfo strategy so one successful redaction path cannot return another secret-bearing component. See `redacted_url_for_display` in src/git.rs.
- **[cache] Object path identity**: Local cache paths use only the normalized SHA-256 OID under two-level shards; pointer size is verified during ingest/materialization rather than encoded in the filename. See `LocalCacheLayout` in src/local_cache.rs.
- **[cache] Verified ingest**: Repository-local Git LFS objects are stream-hashed while copying into a no-clobber temp-file publication path; existing cache files and publish races are reverified instead of overwritten. See `ingest_git_lfs_object` in src/local_cache.rs.
- **[cache] Windows durable sync**: Reopen an existing verified cache object read/write before `sync_all` on Windows; a read-only handle reports access denied even when the file itself is writable. See `sync_verified_cache_object` in src/local_cache.rs.
- **[cache] Worktree registry**: Local cache GC must start from absolute registered worktree roots in `worktrees.json`; relative paths are rejected so future GC does not depend on process cwd. See `LocalCacheWorktreeRegistration` in src/local_cache.rs.
- **[cache] Registry path encoding**: Persist platform-native path units in the current worktree-registry schema and keep v1 UTF-8 reads compatible, so valid non-UTF-8 Unix worktrees remain GC roots. See `serialize_worktree_registry_path` in src/local_cache.rs.
- **[cache] Worktree registry writes**: Register/remove operations hold a separate `worktrees.json.lock` file across load-mutate-save so concurrent LFS Cloud processes cannot drop each other's GC roots. See `LocalCacheLayout::lock_worktree_registry` in src/local_cache.rs.
- **[cache] GC operation boundary**: Cache ingest, materialization, hydration, and dehydration hold a shared `objects.lock`, while GC holds it exclusively; dehydration must retain the lock through pointer publication so GC cannot delete the newly preserved only copy. See `LocalCacheLayout::dehydrate_file` in src/local_cache.rs.
- **[cache] Worktree root identity**: Worktree registration comparisons use canonical path keys when the path exists, so symlinked roots update/remove the same registry row instead of creating duplicate logical worktrees. See `normalized_path_key` in src/local_cache.rs.
- **[cache] Materialization safety**: Cache-to-worktree materialization verifies the cache source first, treats macOS CoW as successful only when `fclonefileat` confirms a clone, and replaces existing worktree files only when they are exact cached bytes or matching LFS pointers. See `materialize_verified_object` in src/local_cache.rs.
- **[cache] Materialization publishing**: Pointer hydration must preserve the replaced worktree file mode and bound pointer reads before parsing, so executable LFS paths stay executable and dirty large files cannot be loaded as pointers. See `read_lfs_pointer_file` in src/local_cache.rs.
- **[cache] New materialization permissions**: Fresh Unix paths created directly from cache default to owner-only `0600`; pointer hydration preserves the existing worktree mode instead, so publication neither bypasses a restrictive umask nor drops executable bits. See `materialize_verified_object` in src/local_cache.rs.
- **[cache] Dehydration traversal**: Hash uncached worktree bytes while staging the cache copy, then verify the atomically displaced bytes as the decisive final identity check; separate pre-copy/pre-exchange hashes add large-file I/O without strengthening concurrent-edit rollback. See `LocalCacheLayout::dehydrate_file` in src/local_cache.rs.
- **[cache] CLI dehydration identity**: Dehydrate only contained Git-tracked `filter=lfs` paths, derive identity from the index pointer, and republish verified cache bytes into repository Git LFS media so later pushes do not depend on private cache state. See `indexed_lfs_object_for_dehydration` in src/cli.rs.
- **[cache] Local GC reachability**: Local cache GC is pointer-reachability based: it scans registered worktrees for Git LFS pointers and may remove cached bytes for hydrated files that no longer have pointer placeholders, since those bytes can be re-preserved from the worktree. See `LocalCacheLayout::garbage_collect` in src/local_cache.rs.
- **[cache] Unavailable GC roots**: A missing registered worktree may be a disconnected volume, so GC protects all possibly referenced objects unless the operator explicitly requests `--prune-unavailable-worktrees`. See `LocalCacheLayout::garbage_collect` in src/local_cache.rs.
- **[cache] CLI registry refresh**: CLI `hydrate` and `dehydrate` require current Git worktree registration before cache mutation; `gc` refreshes when run inside a worktree but still runs as cache administration outside one. See `register_current_worktree_for_gc` in src/cli.rs.
- **[cli] Pull media root**: `pull` resolves Git LFS media objects from `lfs.storage` or `git-common-dir` instead of per-worktree `.git`, because linked worktrees/custom LFS storage fetch into shared media dirs. See `git_lfs_objects_dir` in src/cli.rs.
- **[cli] Pull candidate scope**: `pull` filters tracked checkout paths through `git check-attr filter` before pointer parsing, so docs/fixtures pointer text not tracked by LFS are not hydrated. See `current_checkout_lfs_pointer_files` in src/cli.rs.
- **[cli] Pull path streams**: Keep pull path scans as raw NUL-delimited bytes and write `git check-attr` stdin concurrently with stdout collection, so non-UTF-8 paths and large tracked sets do not fail or deadlock. See `current_checkout_lfs_tracked_paths` in src/cli.rs.
- **[cli] Pull fetch subprocess**: Drain `git lfs fetch` stdout/stderr concurrently with hard retention caps and own its process tree, so noisy, stalled, or pipe-inheriting descendants cannot exhaust memory or keep `pull` alive indefinitely. See `run_bounded_child_command` in src/cli.rs.
- **[build] Rust MSRV**: Keep `Cargo.toml` `rust-version` at least `1.88` while the crate uses Rust 2024 plus let-chain syntax in cache dehydration paths. See `LocalCacheLayout::dehydrate_file` in src/local_cache.rs.
- **[auth] Config debug secrecy**: `ServerConfig` debug output can cross logs and test failures, so provider configs that retain resolved PATs must redact custom `Debug` fields instead of deriving them. See `GitHubProviderConfig` in src/server_config/providers.rs.
- **[auth] Stable repository identity**: Persist GitHub's numeric repository ID in each mapping and compare it with the current owner/name before permission checks, so rename or name reuse cannot retarget an existing LFS namespace. See `GitHubRepositoryPermissionClient::verify_repository_identity` in src/github_auth.rs.
- **[auth] GitHub identity casing**: Match GitHub host/owner/repository identities without ASCII case sensitivity while preserving their original spelling for display and provider calls; keep the Git LFS protocol suffix case-sensitive. See `ServerConfig::repository_mapping_for_identity` in src/server_config/config.rs.
- **[auth] Stable user identity**: Require GitHub's numeric user ID at login and compare it with the permission response's nested user ID, so a renamed or reused login cannot authorize a different account. See `GitHubRepositoryPermissionClient::check_permission` in src/github_auth.rs.
- **[auth] Protected HTTP transport**: URLs carrying PATs, LFS credentials, or object bytes require HTTPS or an exact literal loopback IP; trusted-LAN HTTP needs explicit server and client unsafe opt-ins, and token-bearing GitHub clients never follow redirects. See `uses_protected_http_transport` in src/http_transport.rs.
- **[config] Shared HTTP route bases**: Server public/provider URLs and CLI server bases must use the shared raw-input validator so URL-parser normalization cannot make whitespace, backslashes, or dot segments differ across entry points. See `validate_http_url` in src/http_transport.rs.
- **[config] Editable raw values**: Configuration commands edit the YAML value tree so environment references survive, then validate the complete rendered config and atomically replace it with original permissions; provider removal therefore fails while mappings reference it. See `EditableServerConfig` in src/config_edit.rs.
- **[auth] Durable session key**: Production sessions persist only local-token hashes and AEAD-protected user GitHub PATs; encrypt them with the dedicated server session secret, which must stay stable until active sessions expire or are removed. See `production_session_store` in src/server/runtime.rs.
- **[auth] Session revocation ordering**: Logout revokes the server session before rejecting the repository-scoped Git credential; unexpected server failures preserve the credential for retry, while definitive upstream authentication rejection revokes the local session automatically. See `revoke_lfs_session_route` in src/server.rs.
- **[auth] Session admission**: Bound successful issuance by stable provider identity and process-wide capacity; overload must return a retryable error rather than evict another active credential. See `LocalLfsSessionStore` in src/sessions.rs.
- **[migration] Discovery without git-lfs dependency**: Migration discovery reads repo-scoped config and `.gitattributes` directly so planning still reports filters, endpoints, and LFS patterns when the `git lfs` command is absent. See `discover_git_lfs_migration` in src/migration.rs.
- **[migration] Gitattributes comments**: Git treats only line-start `#` as a `.gitattributes` comment; inline `#` tokens are invalid attributes, not comment delimiters, so discovery should not strip them as comments. See `split_gitattributes_line` in src/migration.rs.
- **[migration] Checkout pointer scope**: Current-checkout migration uses `git check-attr` on index paths and parses index blobs, so Git's attribute precedence decides LFS ownership while hydrated and sparse-omitted worktree files remain discoverable. See `enumerate_current_checkout_lfs_pointers` in src/migration.rs.
- **[migration] Partial-clone dry runs**: Read-only migration Git commands set `GIT_NO_LAZY_FETCH=1`; missing promisor blobs fail explicitly so planning cannot transfer repository data. See `read_only_git_command` in src/migration.rs.
- **[migration] Shallow history scope**: Selected-ref and all-ref inventories fail closed when Git reports a shallow repository; only current-checkout inventory is complete without older history. See `require_complete_history` in src/migration.rs.
- **[migration] Source remote identity**: Bind endpoint discovery, source fetches, and remote-tracking scans to one explicit source remote; require acknowledgement when its provider repository identity differs from target `origin`. See `run_migrate_from_dir` in src/cli.rs.
- **[migration] Dry-run readiness scope**: Dry runs report local Git LFS/filter prerequisites separately and surface remote permission, quota, and capacity as explicit unknown warnings; provider permission and Drive root probes would violate the read-only planning boundary. See `migration_readiness_checks` in src/cli.rs.
- **[migration] Dry-run destination scope**: Local source availability does not prove a target upload; dry runs must report destination existence as unknown because execution performs the provider check and skips existing objects. See `write_migration_dry_run_report` in src/cli.rs.
- **[migration] Default inventory scope**: Current-checkout migration reads only the current index, so its report must warn that other refs were not scanned and direct full provider moves to `--all-refs`. See `MigrationScanMode::scope_warning` in src/cli.rs.
- **[migration] History pointer scope**: Selected/all-ref migration evaluates only pointer-shaped candidates with `git check-attr --source=<commit>`, caching decisions by historical attribute blobs and paths so historical ownership stays exact without repeating unchanged queries. See `HistoryScanner::scan_commit` in src/migration.rs.
- **[migration] Historical Git version**: Selected/all-ref scans require Git 2.40.0 for `git check-attr --source`; preflight before history work and keep current-checkout planning as the older-Git fallback. See `require_historical_scan_git_version` in src/migration.rs.
- **[migration] History tree reuse**: Traverse historical trees through one `cat-file --batch-command` process and cache summaries by tree/blob ID; recursive `ls-tree` per commit repeats every unchanged subtree and makes all-ref scans scale with commits times repository size. See `HistoryScanner::tree_summary` in src/migration.rs.
- **[migration] History scan dedupe**: History scans cache per-commit pointer occurrences and dedupe pointer records by commit/path/object across refs, so aliases and overlapping histories do not inflate migration inventories. See `HistoryScanner::scan_ref` in src/migration.rs.
- **[migration] History gitlinks**: Historical tree scans must skip mode `160000` before pointer parsing; gitlinks can match broad LFS attributes but point to commits rather than blob payloads. See `HistoryScanner::tree_summary` in src/migration.rs.
- **[migration] Git output admission**: Apply command-specific stdout limits while concurrently draining Git pipes, and stop the owned process tree on the first excess byte; checking captured `Command::output` afterward has already allowed unbounded allocation. See `run_bounded_command_output` in src/migration.rs.
- **[migration] Local availability scope**: Check stock Git LFS media first and hash the shared cache only as a fallback; still re-hash the selected source immediately before upload because an earlier availability snapshot can become stale. See `check_local_migration_objects` in src/migration.rs.
- **[migration] Source fetch boundary**: Migration source fetches populate Git LFS media storage with `git lfs fetch` and report post-fetch availability; clear include/exclude filters where compatible and override every recent-fetch setting so repository config cannot widen the chosen ref scope or conflict with `--all`. See `fetch_missing_migration_objects` in src/migration.rs.
- **[migration] Source fetch cleanup**: Git LFS descendants can escape the owned process group while retaining pipes, so cleanup must drain for a bounded grace and join only readers whose completion events arrived; post-exit drains must preserve hard-limit errors before applying inherited-pipe policy. See `wait_for_child` in src/child_process.rs.
- **[migration] Upload verification**: Migration uploads are idempotent destination writes, but still re-hash local source bytes immediately before provider upload and reject provider-returned object/provider mismatches. See `upload_migration_objects_to_storage` in src/migration.rs.
- **[tests] Migration upload simulation**: `verify-migration-upload-simulation.sh` exercises migration uploads only through the fake storage-provider boundary; do not cite it as live Google Drive verification. See `FakeMigrationStorageProvider` in src/migration.rs.
- **[migration] Purge verification boundary**: A dry-run inventory proves only planned scope, not destination upload; withhold purge input until successful execution and an independent destination inventory verification. See `write_migration_source_purge_report` in src/cli/migration.rs.
- **[migration] Execution reconfiguration boundary**: Refresh source refs, require all-ref inventory, reconcile server state, then fetch and upload every target-missing object before changing either Git config location; a partial target result must leave source routing intact. See `run_migrate_execution_from_dir` in src/cli/migration.rs.
- **[migration] Historical endpoint persistence**: Commit the legacy source as `remote.<name>.lfsurl` beside the target `lfs.url` in `.lfsconfig`, then also write the target locally; follow-up migrators can still fetch target-missing objects from the source without direct storage access. See `run_migrate_execution_from_dir` in src/cli/migration.rs.
- **[compatibility] Local Git URL rewrites**: When `url.*.insteadOf` expands a safe configured provider URL to a local/file mirror, retain the configured identity for routing while Git uses the rewritten transport; never apply this fallback to credentialed or other unsafe expanded URLs. See `GitRepository::discover_with_remote` in src/git.rs.
- **[tests] Local fake E2E**: The local end-to-end coverage routes fake GitHub/Drive through LFS batch and object transfer handlers before cache hydration, so it verifies server auth/route behavior without real providers or git-lfs. See `local_init_upload_download_and_checkout_flow_uses_fake_providers` in tests/local_end_to_end.rs.
- **[tests] Production server composition**: Exercise config loading, metadata sync, durable sessions, OAuth/Drive clients, listener binding, and shutdown through `ServerBuilder`; router-only tests cannot detect broken boot-time wiring. See `ServerBuilder` in src/server.rs.
- **[logging] Server diagnostic boundary**: Request handlers log stable repository/object fields and error categories, never rendered request/provider errors, raw request paths, or backend IDs; capture emitted tracing events to enforce this even when an upstream error accidentally retains a secret. See `server_error_log_category` in src/server.rs.
- **[compatibility] Git LFS action auth**: The reference client does not inherit batch-request credentials for advertised transfer URLs; attach the local session credential to every authenticated upload/download action. See `with_session_action_authorization` in src/server.rs.
- **[tests] Shared integration support**: Files under `tests/` compile as separate crates, so reusable `tests/support` helpers may be intentionally unused per crate and should suppress dead-code warnings at the support-module boundary. See `tests/support/mod.rs`.
- **[providers] Repository storage namespace**: Every generic storage operation and returned object carries the stable `RepositoryMapping::id`; fakes and migration checkpoints must key identical OIDs by provider plus repository namespace. See `StorageProvider` in src/providers.rs.
- **[auth] Credential helper process groups**: Credential Git commands need the shared owned process group and joined pipe readers; a successful direct child does not imply helper descendants released inherited stdout/stderr. See `git_command` in src/credentials.rs and `wait_for_child` in src/child_process.rs.
- **[auth] Captured child diagnostics**: Child stdout and stderr can contain credential tokens even when domain errors later redact them, so shared process output and timeout errors must report byte lengths rather than raw bytes in `Debug`. See `ChildProcessOutput` in src/child_process.rs.
- **[metadata] Async SQLite boundary**: Keep startup metadata work synchronous, but dispatch request-path writes through Tokio's blocking pool so SQLite busy waits and the connection mutex cannot block async workers. See `MetadataDatabase::record_verified_object_async` in src/metadata/objects.rs.
- **[storage] Drive proxy integrity**: HTTP downloads proxy Drive chunks directly to avoid disk staging; SHA-256 can only be decided at EOF, so an integrity mismatch terminates the response stream after headers rather than returning a new JSON error. See `GoogleDriveObjectStore::download_object_response` in src/google_drive.rs.
- **[cache] Worktree replacement races**: Serialize same-path cache operations and use exchange rename to verify displaced worktree bytes after publication; if rollback fails, retain the displaced file at a reported recovery path. See `replace_retaining_displaced` in src/local_cache.rs.
- **[cache] Worktree symlink boundary**: Canonicalize the requested parent inside the current worktree, reject final symlinks, and use no-follow reads through publication verification so hydration/dehydration never replace a link after hashing its target. See `open_worktree_file_without_following_symlinks` in src/local_cache.rs.
- **[cache] GC reachability source**: Enumerate registered worktrees through NUL-delimited Git tracked paths and index `filter=lfs` attributes; raw filesystem pointer scans let ignored, generated, or untracked text pin cache objects indefinitely. See `collect_tracked_lfs_pointer_oids` in src/local_cache.rs.
- **[cache] GC pointer opens**: Inspect tracked pointer candidates before opening them and use nonblocking Unix opens, because worktree files can become FIFOs or symlinks between Git enumeration and pointer reads. See `collect_pointer_oid_from_file` in src/local_cache.rs.
- **[compatibility] Shared Git path output**: Parse NUL-delimited `check-attr` triples and enforce raw Unix path plus relative-containment rules in one boundary so pull, cache GC, and migration cannot drift. See `parse_lfs_filter_attribute_paths` in src/git_output.rs.
- **[tests] Process environment isolation**: A module-local mutex cannot justify process-environment mutation because unrelated test threads may read it; pass environment cases to an exact ignored test in a child process instead. See `configured_filter_environment_subprocess` in src/logging.rs.
- **[tests] Throwaway temp isolation**: Plain temporary directories under `~/Sites/throwaway` inherit its Git root and invalidate non-repository cases; use nested repos there only for explicit repo workflows and leave general tempdirs outside it. See `baseEnv` in .agents/skills/smoke-test/scripts/smoke-test.ts.
- **[tests] Throwaway root defaults**: Preserve the local macOS smoke root at `~/Sites/throwaway`, but default Windows to `~/Projects/throwaway`; CI and containers should continue supplying `LFS_CLOUD_SMOKE_THROWAWAY`. See `defaultThrowawayRoot` in .agents/skills/smoke-test/scripts/smoke-test.ts.
- **[tests] Windows Python aliases**: Git Bash can resolve a nonfunctional Microsoft Store `python3` alias before a working `python`; manual verifiers must execute and version-check candidates through `lfscloud_find_python3`. See `lfscloud_find_python3` in scripts/lib/python.sh.
- **[tests] Native Git LFS smoke fixtures**: Git for Windows bypasses extensionless `git-lfs` Bash fakes launched by native children; test pull fetches with real Git LFS and a local `remote.origin.lfsurl` while keeping a provider-valid GitHub origin. See scripts/manual/verify-pull-command.sh.
- **[tests] Git Bash YAML paths**: MSYS converts command arguments but not paths embedded inside generated YAML; use `cygpath -m` before writing Git Bash temp paths into Windows-consumed configuration. See scripts/manual/verify-status-command.sh.
- **[build] Artifact smoke boundary**: CI must pass `LFS_CLOUD_SMOKE_BINARY` after target tests and the release build; manual CLI verifiers honor that path so smoke checks exercise the uploaded artifact instead of a Cargo debug rebuild. See `run_lfscloud` in scripts/lib/lfscloud-command.sh.
- **[security] Dependency advisory ownership**: Run the pinned RustSec audit on dependency changes and weekly; maintainers own failures and warning triage, and may ignore an advisory only with reachability evidence, a tracking issue, and a review or expiry date. See `.github/workflows/dependency-audit.yml`.
- **[security] CI secret rotation**: Keep live-provider values aligned between ignored `.env.local` and GitHub Actions, and stream values to `gh` over stdin so secrets do not enter process arguments. See `rotation_sync_github_secret` in scripts/lib/key-rotation.sh.
- **[release] Local status provenance**: Release only an exact origin commit whose latest required `local-checks/*` statuses are successful and owned by the active GitHub user; rerun the exact release binary before tagging. See `release_require_local_statuses_green` in scripts/lib/release-common.sh.
- **[release] Interrupted publication**: A failed post-push release leaves immutable history intact; `release:local resume` continues the current version/tag instead of incrementing again or rewriting a pushed commit. See `scripts/release.sh`.
- **[release] Draft publication boundary**: Assemble macOS, Linux, Debian, installer, and Windows assets in an editable draft; selecting it in macOS-native `release:publish` is the publication decision after all four trusted local statuses are green, while channel statuses make immutable-release distribution resumable. See `scripts/publish.sh`.
- **[release] Publisher status provenance**: Query the plural commit-status endpoint during candidate selection; GitHub's combined status response omits creator data and makes green locally verified releases appear untrusted. See `publish_commit_status_document` in scripts/publish.sh.
- **[release] Optional APT distribution**: Include `distribution/apt` in completion only when `LFS_CLOUD_APT_CLOUDSMITH_TARGET` is set; configuring it later makes an immutable release with missing APT status resumable. See `publish_distribution_contexts` in scripts/publish.sh.
- **[release] Homebrew retry boundary**: Trust the configured tap before loading its formula, and accept a dirty tap only when its sole change is the exact regenerated `Formula/lfscloud.rb`. See `publish_homebrew_checkout_is_resumable` in scripts/publish.sh.
- **[release] WinGet fork remotes**: Clone the authenticated fork with `gh repo clone --no-upstream` before adding the upstream remote explicitly; GitHub CLI otherwise adds it automatically and the retry fails immediately. See `publish_clone_winget_fork` in scripts/publish.sh.
- **[release] WinGet manifest remediation**: Emit the 1.12 schema header in every generated manifest and update an existing open submission branch before treating its PR as complete, so reviewer changes remain resumable. See `publish_write_winget_manifests` in scripts/publish.sh.
- **[release] Direct installer ownership**: Direct installers record ownership beside the binary and refuse to replace an unmanaged executable unless forced, so direct updates cannot silently overwrite package-manager installs. See `scripts/install.sh` and `scripts/install.ps1`.
- **[build] Reusable Linux checks**: Keep Linux verification in explicitly named architecture-specific containers with a shared Cargo source cache and separate target volumes; sharing target output across macOS or Linux architectures can reuse incompatible host build artifacts. See `verify_linux_docker` in scripts/lib/verify-linux-docker.sh.
- **[build] Linux verifier incrementality**: Disable Cargo incremental compilation in persistent Linux verification targets; dependency and final artifacts remain reusable, while incremental sessions otherwise retain several gigabytes per architecture for an edit-build workflow the pushed-commit verifier does not perform. See scripts/docker/run-linux-verification.sh.
- **[scripts] Terminal status reporting**: Release and verification scripts initialize `terminal-ui.sh` through `release-common.sh`; parallel verifiers use bounded rolling slots, and every entrypoint finalizes UI state from its exit trap so subprocess logs cannot corrupt live terminal regions. See `ui_enable_rolling_slots` in scripts/lib/terminal-ui.sh.
- **[scripts] Publisher error visibility**: Send fatal publisher messages through persistent terminal UI logging; raw stderr written while its live region is active is cleared during finalization. See `publish_error` in scripts/publish.sh.
- **[tests] Emulated timeout margin**: Network timeout tests must leave enough dispatch margin for emulated ARM64 runners while keeping the mock stall substantially longer; otherwise a client attempt can expire before the mock server records it. See `object_store_times_out_a_stalled_upload_response` in src/google_drive.rs.
