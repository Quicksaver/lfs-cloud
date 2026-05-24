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
- **[auth] Callback CSRF registry**: The callback router must consume a registered state per authorization attempt, not store one fixed expected state, so long-lived routers support concurrent logins and reject replay. See `GitHubOAuthStateRegistry` in src/github_auth.rs.
- **[auth] Callback state replay**: Consume registered callback states before handling provider-denied OAuth errors, and keep the registry TTL-pruned and bounded so abandoned login attempts cannot be replayed or grow memory without limit. See `GitHubOAuthStateRegistry` in src/github_auth.rs.
- **[auth] Local LFS sessions**: Git LFS credentials are separate `lfs-cloud` bearer tokens backed by local session metadata; never store or hand GitHub OAuth access tokens to Git credential-helper paths. See `LocalLfsSessionStore` in src/sessions.rs.
- **[auth] GitHub permission denial**: Treat GitHub collaborator `404`, `none`, SSO-required, and unknown permission states as authorization denials; do not convert the permission endpoint's `404` into repository-not-found. See `GitHubRepositoryPermissionClient` in src/github_auth.rs.
- **[auth] Git credential path scope**: Persist `credential.<lfs-host>.useHttpPath=true` before approving tokens; one-shot `git -c` scoping affects storage but not later Git LFS lookups, which can leak host-matched credentials across repo paths. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential helper preflight**: Check `git config --get-urlmatch credential.helper <lfs-url>` before `git credential approve`; Git can accept an approve request with no helper configured and then persist nothing. See `GitCredentialApproval` in src/credentials.rs.
- **[auth] Credential URL safety**: Git credential-helper URLs become config keys and protocol input, so reject userinfo and query strings rather than trying to sanitize them later. See `validate_lfs_credential_url` in src/credentials.rs.
- **[auth] Credential lookup scope**: Credential fill must prove protocol, host, path, and username match the configured LFS URL before accepting a stored token, so a host-scoped helper entry cannot satisfy another repo path. See `GitCredentialLookup` in src/credentials.rs.
- **[storage] Drive credential refs**: Bare Drive `credentials_ref` values map to prefixed env vars containing flat OAuth JSON; use `env:NAME` only when the server operator needs an explicit secret variable. See `GoogleDriveCredentialLoader` in src/google_drive.rs.
- **[storage] Drive token URI safety**: Custom Google OAuth `token_uri` values must use HTTPS and cannot carry query strings or fragments; HTTP is reserved for loopback-only test endpoints. See `validate_token_url` in src/google_drive.rs.
- **[storage] Drive root scope**: The MVP uses `drive.file`; configured root folders must be app-created or explicitly app-accessible, and startup/health checks should validate folder type plus child-write capability before transfers. See `GoogleDriveRootValidator` in src/google_drive.rs.
