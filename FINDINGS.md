# Implementation Review Findings

These findings were consolidated directly from focused subagent reviews and
deduplicated by merging reports of the same underlying issue. They have not been
independently validated or adjudicated.

## Authentication and credential security

1. **[DONE] Repository name reuse can expose the original repository's LFS
   objects** (High, `src/server_config.rs`, `src/server.rs`, and
   `src/github_auth.rs`): **Valid and actionable.** Production previously built
   `RepositoryIdentity` with no stable ID and authorized the mutable
   `owner/name` directly. Repository mappings now require the provider's stable
   repository ID; GitHub mappings validate it as a positive numeric ID, the
   production authorizer carries it into `RepositoryIdentity`, and every GitHub
   permission decision first resolves `/repos/{owner}/{repo}` and denies a
   missing or mismatched ID as repository-not-found. Configuration examples and
   operator guidance now document how to persist the ID and preserve it across
   renames. Regression coverage proves that a replacement repository with an
   admin permission response is still denied when its numeric ID differs, and
   config tests cover missing and malformed IDs. The focused reviewer found a
   genuine high-severity authorization boundary flaw and prescribed the right
   stable-identity defense; with one valid finding assessed here and no invalid
   finding attributable separately, this was high-quality, relevant feedback.

2. **[DONE] OAuth tokens are not bound to the session user's stable identity**
   (High, `src/github_auth.rs` and `src/server.rs`): **Valid and actionable.**
   Login already retained GitHub's numeric user ID when present, but accepted
   identity responses without it; repository authorization then used only the
   mutable login and ignored the permission response's nested `user.id`.
   GitHub login now requires a positive numeric ID, permission checks require a
   valid numeric session ID, and access is granted only when the collaborator
   response identifies the same stable user. Missing response identity is
   treated as malformed upstream data, while a mismatched ID is denied without
   exposing either ID. Regression coverage verifies missing login IDs, login
   reuse by a different stable user, and the production batch-authorizer path
   with a token/session user mismatch. The GitHub REST collaborator schema was
   checked to confirm that successful permission responses include nested user
   identity. The focused reviewer found a genuine high-severity identity
   binding flaw and recommended the correct stable-ID comparison; with one
   valid finding assessed here and no invalid findings attributable separately,
   this was high-quality, security-relevant feedback.

3. **[DONE] Repository-local Git configuration can override credential path
   isolation** (High, `src/credentials.rs`, `src/cli.rs`, and credential manual
   verification scripts): **Valid and actionable.** Real Git reproduced the
   precedence flaw: a repository-local `useHttpPath=false` remained effective
   despite the previous global `true`, allowing plain credential approval to
   discard the LFS repository path. Credential approval now accepts an explicit
   repository context, runs the helper preflight and approval there, and writes
   the URL-matched `useHttpPath=true` into that repository's local config before
   exposing the token to the helper. The production login path passes its
   discovered current repository explicitly. A real-Git regression starts with
   the hostile local `false`, approves through an isolated `credential-store`
   file, verifies the effective value became `true`, and proves the persisted
   credential retains the full repository LFS path. README guidance, the
   repository learning, and both credential/login manual checks now describe
   and exercise repository-local path isolation. Verification passed with
   `cargo fmt`, `yarn lint:fix`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test --all-targets`, `cargo test --doc`,
   `scripts/manual/verify-git-credential-approve.sh`, and
   `scripts/manual/verify-login-command.sh`. The focused reviewer found a
   genuine high-severity credential-isolation flaw, correctly identified Git
   config precedence as the cause, and requested the decisive real-Git
   regression; with one valid finding assessed here and no invalid finding
   attributable separately, this was high-quality, security-relevant feedback.

4. **[DONE] Non-loopback plaintext HTTP can expose authentication secrets and
   LFS object content** (High, `src/http_transport.rs`, `src/server_config.rs`,
   `src/server.rs`, `src/init.rs`, `src/cli.rs`, `src/credentials.rs`,
   `src/github_auth.rs`, and `src/google_drive.rs`): **Valid and actionable.**
   The shared transport policy now requires HTTPS or an exact literal IPv4/IPv6
   loopback address for URLs carrying OAuth, local LFS credentials, provider
   tokens, or object bytes. Server configuration rejects unsafe public and
   GitHub API URLs by default, and startup rejects a plaintext non-loopback bind
   even when the configured public URL is loopback. Trusted-LAN development is
   still possible only through the explicit `server.allow_insecure_http: true`
   setting and matching `--allow-insecure-http` flags on client commands. Git
   credential approval and lookup independently enforce the same boundary so a
   caller cannot persist or retrieve a local LFS token for an unapproved
   plaintext endpoint. Default GitHub OAuth/API and Google provider clients no
   longer follow redirects, preventing token-bearing requests from being
   redirected to a downgraded or unrelated endpoint. Configuration, CLI,
   operator documentation, the repository learning, and the LAN smoke verifier
   describe and exercise the new contract. Verification passed with
   `yarn lint:fix`, `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and
   `scripts/manual/verify-lan-smoke-test.sh`. The focused reviewer found a
   genuine high-severity transport flaw, identified every important secret and
   data path affected, and proposed the correct safe default plus explicit
   development escape hatch; with one valid finding assessed here and no
   invalid finding attributable separately, this was high-quality,
   security-relevant feedback.

5. **[DONE] Server restarts invalidate every issued LFS credential despite the
   SQLite session contract** (High, `src/server.rs`, `src/sessions.rs`,
   `src/metadata.rs`, and session documentation): **Valid and actionable.**
   Production previously created a fresh in-memory session store even after it
   opened the metadata database, so every restart invalidated the local token
   already stored by Git and discarded the private GitHub token required for
   authorization. Production now opens a durable session store on the shared
   metadata database, persists only the local token's SHA-256 digest, restores
   unexpired identity/scope/expiry data, and protects the GitHub token with
   AES-256-GCM using a dedicated key derived from the configured GitHub OAuth
   client secret. Identity, scopes, and timestamps are authenticated as AEAD
   associated data, so tampering or a different secret fails restoration
   without exposing either token. Metadata schema version 3 adds the protected
   token fields and durable load/record/delete/prune operations; in-memory
   stores remain available only for isolated tests and injected routers.
   Restart/reopen coverage exercises both the store and production composition,
   proves neither token appears as plaintext in SQLite, rejects the wrong key,
   and rejects tampered identity metadata. README, configuration,
   implementation, dependency, and repository-learning documentation now state
   the persistence and key-stability contract. Verification passed with
   `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test --all-targets`, `cargo test --doc`,
   `yarn lint:fix`, and `git diff --check`. The focused reviewer found a genuine
   high-severity availability and contract flaw, identified the unused schema
   and missing private-token persistence precisely, and requested the decisive
   restart/reopen regression; with one valid finding assessed here and no
   invalid finding attributable separately, this was high-quality,
   security-relevant feedback.

6. **[DONE] OAuth callback responses containing credentials were cacheable**
   (Medium, `src/github_auth.rs`): **Valid and actionable.** Successful callback
   GET responses return the local LFS bearer token, while the callback router
   previously supplied no cache or referrer policy on either success or error
   responses. The callback router now applies `Cache-Control: no-store, private`,
   `Pragma: no-cache`, and `Referrer-Policy: no-referrer` through response
   middleware. Applying the policy at the router layer also protects callback
   errors produced before the handler returns, including future extractor
   rejections. Regression assertions cover both the credential-bearing success
   response and an unauthorized CSRF error response. Verification passed with
   `cargo test callback_route_`, `cargo fmt`, `yarn lint:fix`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, and `cargo test --doc`. The focused reviewer
   identified a genuine medium-severity browser and intermediary cache exposure
   and prescribed the correct defensive headers; with one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, security-relevant feedback.

7. **[DONE] Unauthenticated login flooding could evict legitimate CSRF
   states** (Medium, `src/github_auth.rs` and `src/error.rs`): **Valid and
   actionable.** The pending-state registry previously admitted the 1,025th
   login by silently deleting the oldest unrelated attempt, and every callback
   scanned the full registry while holding its mutex. Registration now hashes
   each opaque random state into a fixed digest key, consumes it through direct
   `HashMap` lookup, and rejects new admission at the bounded capacity without
   evicting any active attempt. Capacity exhaustion is a typed server
   rate-limit error; the login endpoint returns HTTP 429 with `Retry-After`
   instead of redirecting with a state that displaced another user. The
   registry clock is injectable in tests, and deterministic coverage proves
   validity immediately before expiry, expiry at the exact TTL, continued
   expiry afterward, capacity reuse, exact one-time lookup, overload response
   headers, and preservation of an unrelated active state. The implementation
   intentionally retains server-side one-time random states rather than adding
   a second cookie/signature mechanism: they already provide unguessable CSRF
   binding and replay consumption, while digest-keyed lookup removes the linear
   scan. It also avoids a naive global requests-per-second throttle, which an
   unauthenticated caller could monopolize; per-client temporal throttling
   remains an appropriate listener or trusted-proxy control once the server has
   a trustworthy client-identity boundary. Verification passed with
   `cargo fmt`, focused state-registry and 429 route tests,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, and `cargo test --doc`. The focused reviewer
   identified a genuine medium-severity availability flaw, correctly connected
   eviction and linear lookup to unauthenticated traffic, and requested the
   decisive boundary and preservation tests. The cookie/signed-state and
   endpoint-throttle suggestions were broader than necessary for this
   server-side one-time-state design, but the core finding and remediation
   direction were high-quality and security-relevant; with one valid finding
   and no invalid finding attributable separately, this was strong feedback.

8. **[DONE] Successful logins could evict unrelated active sessions** (Medium,
   `src/sessions.rs`): **Valid and actionable.** At the process-wide
   1,024-session cap, issuance previously deleted the soonest-expiring active
   session, including its durable SQLite row, before admitting the new login.
   Session admission now rejects overload with a retryable rate-limit error and
   never evicts an active credential. It allows at most 16 active sessions and
   eight successful issuances per minute for each stable provider user, while
   retaining the 1,024-session process-wide cap. Stable IDs, rather than mutable
   logins, define principals when available. The store now owns injectable clock
   and token-generation functions, allowing deterministic tests to prove exact
   expiry and issuance-window boundaries, expired-capacity pruning, stable-user
   rename handling, independent cross-user capacity, overload preservation of
   every active token, and concurrent issue/verify/revoke behavior without
   wall-clock sleeps. README and implementation guidance document the limits,
   HTTP 429/`Retry-After` behavior, and non-eviction contract, while the
   repository learning records the admission boundary. Verification passed
   with `cargo fmt`, `yarn lint:fix`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1`, and `cargo test --doc`. An
   initial default-parallel all-target run exposed seven unrelated,
   load-sensitive credential-helper timeout failures; the complete credential
   module then passed 31/31, and the serial all-target rerun passed. The focused
   reviewer identified a genuine medium-severity cross-user availability flaw,
   correctly connected eviction and unbounded per-user issuance, and requested
   the decisive deterministic and concurrency coverage. With one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, security-relevant feedback.

9. **[DONE] Token-endpoint errors could reflect submitted OAuth secrets and
   accept unbounded bodies** (Medium, `src/github_auth.rs`): **Valid and
   actionable.** Token exchange previously decoded successful JSON responses
   and read unsuccessful response text without a byte limit. Its diagnostic
   sanitizer removed control characters and truncated text, but never redacted
   the submitted client secret or authorization code if GitHub reflected them.
   Both successful and unsuccessful token responses now pass through a strict
   16 KiB reader that rejects an oversized declared or streamed body before
   parsing it. Structured JSON/form OAuth diagnostics and unstructured upstream
   bodies now redact the client secret and validated callback code before the
   existing diagnostic normalization and truncation, with longer overlapping
   secrets redacted first. Regression coverage exercises JSON, form-encoded,
   and unstructured reflection plus oversized success and error bodies.
   Verification passed with `yarn lint:fix`, `cargo fmt --check`, focused
   token-exchange tests, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test --all-targets`, `cargo test --doc`, and
   `git diff --check`. The focused reviewer found a
   genuine medium-severity secret-disclosure and resource-boundary flaw,
   correctly identified redaction ordering as security-relevant, and requested
   the decisive response-format regressions; with one valid finding assessed
   here and no invalid finding attributable separately, this was high-quality,
   security-relevant feedback.

10. **[DONE] Failed credential lookup could disclose a stored token** (Medium,
    `src/credentials.rs` and
    `scripts/manual/verify-secret-redaction.sh`): **Valid and actionable.** A
    failing `git credential fill` process previously surfaced raw helper stderr
    while the stored password was available only in the helper's stdout. On a
    failure or timeout, lookup therefore had no reliable secret value to pass
    to exact-token redaction, and an arbitrary helper diagnostic could expose
    the credential. Credential lookup now sends helper stderr directly to the
    null stream and rewrites command failure and timeout diagnostics to a fixed
    safe message. Other credential approval and Git configuration commands
    retain their more informative exact-token redaction because those paths
    already know the submitted token. A fake-helper regression writes a
    sentinel stored token to stderr before failing and proves neither the token
    nor surrounding helper text reaches the returned error; the regression is
    also required by the manual secret-redaction gate. The repository learning
    records why lookup diagnostics require suppression rather than ordinary
    redaction. Verification passed with `cargo fmt`, `yarn lint:fix`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`,
    `scripts/manual/verify-secret-redaction.sh`, and `git diff --check`. The
    focused reviewer identified a genuine medium-severity credential disclosure
    boundary and recommended both the safe suppression option and the decisive
    failing-helper regression; with one valid finding assessed here and no
    invalid finding attributable separately, this was high-quality,
    security-relevant feedback.

11. **[DONE] Credential lookup was not non-interactive** (Medium,
    `src/credentials.rs`, `README.md`, and `AGENTS.md`): **Valid and
    actionable.** Credential fill previously inherited the caller's prompt
    environment, so a local-token cache miss could invoke Git's configured
    askpass program, fall back to a terminal prompt, or allow Git Credential
    Manager to open terminal or GUI authentication. Lookup now sets
    `GIT_TERMINAL_PROMPT=0`, removes inherited `GIT_ASKPASS` and `SSH_ASKPASS`,
    overrides `core.askPass` to empty for the command, and disables GCM through
    both `credential.interactive=false` and the `GCM_INTERACTIVE=0` and
    `GCM_GUI_PROMPT=0` environment controls. A cache-miss regression captures
    the exact child-process arguments and environment, returns no credential,
    and proves lookup fails immediately without retaining any prompt path. The
    README now documents that status credential checks are non-interactive, and
    the repository learning records why terminal, askpass, and GCM controls
    must be disabled together. Verification passed with `cargo fmt`,
    `yarn lint:fix`, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
    focused reviewer identified a genuine medium-severity unattended-command
    availability flaw and prescribed the correct layered prompt controls plus
    the decisive cache-miss regression; with one valid finding assessed here
    and no invalid finding attributable separately, this was high-quality,
    relevant feedback.

12. **[DONE] The OAuth authorization flow omitted PKCE** (Medium,
    `src/github_auth.rs`, `README.md`, `IMPLEMENTATION.md`, and `AGENTS.md`):
    **Valid and actionable.** The browser authorization previously bound the
    callback only with CSRF state and redirect URI, while code exchange used the
    client secret but no proof tied to the originating attempt. GitHub's
    [OAuth App web-flow documentation](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps)
    strongly recommends S256 PKCE and requires the original verifier at token
    exchange when a challenge was sent. Each authorization now generates a
    fresh 32-byte verifier through the `oauth2` crate, sends its 43-character
    SHA-256 challenge and `S256` method, and transfers exclusive verifier
    ownership into the digest-keyed pending-state registry before redirecting.
    Callback admission removes the matching state and verifier together before
    provider-error handling or code exchange, and the token request includes
    `code_verifier`; reflected verifier values are redacted with the client
    secret and authorization code. Regression coverage proves the challenge is
    derived from the retained verifier, the verifier reaches token exchange,
    an intercepted code paired with another attempt's state/verifier is denied,
    the original attempt still succeeds, and its replay is rejected before a
    second exchange. README and implementation guidance document the PKCE
    contract, while the repository learning records one-owner attempt state.
    Verification passed with `cargo fmt`, `yarn lint:fix`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
    focused reviewer identified a genuine medium-severity authorization-code
    interception defense gap and prescribed the correct state-bound S256 PKCE
    lifecycle plus decisive interception/replay tests; with one valid finding
    assessed here and no invalid finding attributable separately, this was
    high-quality, security-relevant feedback.

13. **Medium — Session revocation has no user-facing route or CLI flow.** The
    `revoke` operation is only exercised by unit tests, so a stolen token remains
    usable until expiry or restart (`src/sessions.rs:342-355`). Expose
    authenticated logout/revocation, erase the Git credential entry, and revoke
    local sessions when upstream authentication is definitively invalid.

14. **Low — Authentication performs a serialized O(n) session scan and clones
    OAuth-bearing records.** Every request locks the map and prunes all sessions
    (`src/sessions.rs:329-340`, `src/sessions.rs:415-420`). Check only the
    requested entry, maintain an expiry index or background pruning task, and
    share records through `Arc`.

15. **Low — GitHub REST requests do not pin an API version.** Requests set
    `Accept` but omit `X-GitHub-Api-Version`
    (`src/github_auth.rs:543-549`, `src/github_auth.rs:644-650`). Configure a
    supported version header centrally and assert it in mocked requests.

16. **Low — Credential-helper descendants can leave blocked reader threads.** A
    successful direct child can exit while a descendant retains stdout or
    stderr, causing drain timeouts and detached blocked threads
    (`src/credentials.rs:832-846`, `src/credentials.rs:983-1020`,
    `src/credentials.rs:1117-1145`). Use cancellable/nonblocking reads or
    deterministic process-group cleanup, and test a pipe-holding descendant.

## Server and metadata

1. **High — Unbounded batches and per-object authorization can amplify into
   thousands of GitHub, OAuth, and Drive API calls.** Object count and duplicates
   are not bounded; each lookup reloads credentials, refreshes a Google token,
   and queries Drive, while a batch and each subsequent GET or PUT can perform
   separate GitHub permission checks, sometimes before cheap local validation
   (`src/server.rs:682-703`, `src/server.rs:840-842`,
   `src/server.rs:976-986`, `src/server.rs:1038-1064`,
   `src/server.rs:1105-1135`, `src/server.rs:1593-1616`,
   `src/server.rs:1656-1707`, `src/lfs.rs:568`). Enforce a configurable object
   count, deduplicate identities, validate locally first, cache Google tokens
   with single-flight refresh, add a server-wide provider-call semaphore, and
   use a short-lived scoped authorization grant or conservative permission
   cache. Test provider-call counts for malformed and multi-object requests.

2. **High — Authenticated batch bodies have no read timeout or global request
   limit.** `Bytes::from_request` can wait indefinitely for a valid session
   slowly dripping a body (`src/server.rs:958-1004`). Add batch-body idle and
   total timeouts plus global connection/request limits, and exercise them
   through the actual router.

3. **High — Upload free-space checks race and do not reserve aggregate
   capacity.** Concurrent uploads can all pass the same preflight and then fill
   the staging filesystem (`src/server.rs:1134-1150`,
   `src/server.rs:1358-1385`, `src/server.rs:1464-1506`). Add global/per-user
   concurrency limits and an atomic weighted byte reservation released with the
   tempfile; retain the filesystem check as a secondary guard.

4. **Medium — Startup does not validate Drive credentials or root-folder
   usability.** The server binds successfully and discovers invalid storage only
   during the first batch lookup (`src/server.rs:93-116`,
   `src/server.rs:672-704`). Validate each provider before declaring readiness,
   or expose readiness that remains unhealthy until validation succeeds.

5. **Medium — Per-object upload locks leak permanently.** Every distinct
   repository/provider/OID inserts a lock that is never removed
   (`src/server.rs:849-891`, `src/server.rs:1149-1151`). Use a race-safe keyed
   lock manager with weak or bounded entries, and test that completed uploads do
   not retain locks.

6. **Medium — Graceful shutdown and transfer draining are absent.** The server
   does not use `with_graceful_shutdown`, so termination can interrupt large
   staged transfers and leave incomplete backend/metadata state
   (`src/server.rs:117-133`). Handle SIGINT/SIGTERM, stop accepting requests,
   and drain transfers for a documented bounded interval.

7. **Medium — Metadata config synchronization retains stale routes and can block
   legitimate renames.** Synchronization only upserts, so a removed mapping can
   retain the unique route and make a renamed mapping fail startup
   (`src/metadata.rs:394-417`, `src/metadata.rs:602-641`). Reconcile active and
   persisted configuration transactionally, using an active/generation marker
   if historical rows must remain.

8. **Medium — Idempotent verification rewrites original uploader attribution.**
   The conflict update preserves `created_at` while replacing every
   `created_by` field (`src/metadata.rs:447-490`,
   `src/server.rs:1163-1190`). Preserve original creator fields and use separate
   last-verified or updated attribution where needed.

9. **Medium — Newer metadata schemas are silently accepted and may be
   modified.** Initial schema statements execute before the code validates
   `PRAGMA user_version`, and versions above the supported version are not
   rejected (`src/metadata.rs:333-367`). Read and validate the version before
   mutation and add a future-version regression test.

10. **Medium — Synchronous SQLite and a standard mutex block async request
    workers.** Upload completion directly performs serialized synchronous
    database work on Tokio workers (`src/metadata.rs:266-269`,
    `src/metadata.rs:429-521`, `src/server.rs:750-765`). Move operations to
    `spawn_blocking`, `tokio-rusqlite`, or a bounded connection pool.

11. **Low — Size-only integrity failures incorrectly report a SHA-256
    mismatch.** A correct OID with the wrong size receives a misleading hash
    error (`src/server.rs:1799-1806`, `src/server.rs:3474-3503`). Report OID and
    size generically or classify them separately, and fix the assertion.

12. **Low — Transfer-attempt metadata is declared but never recorded.** The
    schema and documentation promise transfer state, but production inserts no
    lifecycle rows (`src/metadata.rs:109-129`, `src/server.rs:105-106`,
    `src/server.rs:758-764`). Record sanitized start/success/failure rows or
    remove the table and claim until implemented.

13. **Low — Public server documentation and the base-route error are stale.**
    They still claim transfer handling is for later work even though batch and
    transfer endpoints exist (`src/server.rs:3-8`, `src/server.rs:82-87`,
    `src/server.rs:951-954`). Update the docs and return a base-endpoint-specific
    response.

## Google Drive storage

1. **High — Concurrent or retried uploads can create duplicate Drive files and
   make an object permanently unreadable.** The upload flow has no durable
   idempotency boundary, serialization works only within one server process, and
   lookup treats multiple matching files as a conflict
   (`src/google_drive.rs:840`, `src/google_drive.rs:2103`,
   `src/google_drive.rs:3276`, `src/server.rs:856-890`,
   `src/server.rs:1158-1229`). Enforce and document single-writer operation with
   a durable lock for the MVP, or introduce durable claims/reservations and
   deterministic duplicate reconciliation for multi-instance operation. Test
   concurrent and retried uploads across independent server states.

2. **High — Downloads described as streaming are fully staged before the first
   response byte.** This increases disk use and time to first byte and conflicts
   with the user-facing streaming description. Staging also has no size,
   free-space, timeout, or concurrency guardrails and accepts objects above the
   upload limit (`src/google_drive.rs:977`, `src/google_drive.rs:1003`,
   `src/google_drive.rs:1121`, `src/server.rs:1031-1084`,
   `src/server.rs:3374-3412`, `src/google_drive.rs:3746`,
   `src/server.rs:3249`, `README.md:31`, `IMPLEMENTATION.md:753`). Implement a
   bounded integrity-verified stream or a managed verified cache with weighted
   staging admission, quotas, backend idle timeouts, and concurrency limits.
   Add chunked large-transfer tests covering slow peers, interruption, disk
   exhaustion, tempfile cleanup, and bounded memory; align the documentation.

3. **High — Drive upload and download clients lack connect and per-read idle
   timeouts.** Network stalls can leave token-bearing operations awaiting
   indefinitely (`src/google_drive.rs:853`, `src/google_drive.rs:903`,
   `src/google_drive.rs:1015`, `src/google_drive.rs:1537`,
   `src/google_drive.rs:1558`). Configure connect timeouts and per-read idle
   watchdogs without imposing an inappropriate total timeout on large streams.

4. **Medium — Valid repository IDs can exceed Drive app-property limits.** The
   configuration accepts IDs that, combined with the property key, exceed
   Drive's 124-byte key-plus-value limit (`src/google_drive.rs:596`,
   `src/google_drive.rs:1870`, `src/google_drive.rs:1907`,
   `src/google_drive.rs:2947`). Store a fixed-size digest or validate the true
   boundary and add maximum-length tests. See the [Drive custom-properties
   limits](https://developers.google.com/workspace/drive/api/guides/properties).

5. **Medium — Paginated Drive lookup results are mishandled as conflicts.** The
   lookup URL omits page tokens and does not follow `nextPageToken`
   (`src/google_drive.rs:1682`, `src/google_drive.rs:2103`). Iterate all pages
   before deciding whether the object is missing, unique, or duplicated.

6. **Medium — The resumable upload implementation does not actually resume.** A
   failed upload starts over instead of querying or continuing the existing
   session (`src/google_drive.rs:827`, `src/google_drive.rs:901`,
   `src/google_drive.rs:2307`, `src/google_drive.rs:3589`). Upload in chunks,
   probe the committed offset, retry with bounded backoff, and test interrupted
   sessions.

7. **Medium — Drive error classification omits important quota and permission
   reasons.** Errors that should drive retry, denial, or operator remediation can
   collapse into generic upstream failures (`src/google_drive.rs:2358`,
   `src/google_drive.rs:3911`). Expand classification for documented Drive
   reason codes and add representative tests.

8. **Medium — Flat root-folder storage creates a scaling ceiling and expensive
   list queries.** Every object shares one folder and lookup relies on list
   queries (`src/google_drive.rs:784`, `src/google_drive.rs:1870`). Prefer stored
   backend IDs with direct `files.get`, add deterministic sharding, and define a
   repair path for stale metadata.

## CLI, configuration, and Git integration

1. **High — URL redaction can leak credentials in combined malformed or
   scp-style inputs.** The redactor does not safely compose userinfo, query, and
   scp-like sanitization (`src/git.rs:633-665`, `src/git.rs:785-813`,
   `src/cli.rs:3250-3300`). Parse and redact all sensitive components before
   truncation or display, and add cases combining userinfo, query, fragment, and
   scp-like syntax.

2. **Medium — GitHub owner and repository matching is case-sensitive.** Route
   and configuration identity can diverge for names GitHub treats
   case-insensitively (`src/server_config.rs:175-219`, `src/cli.rs:581-587`,
   `src/cli.rs:1055-1067`, `src/git.rs:529-548`). Normalize GitHub identities for
   comparison while preserving a display form, and add mixed-case tests.

3. **Medium — `pull` can hang and buffers unbounded command output.** Child
   process output is fully captured before truncation or timeout handling
   (`src/cli.rs:1421-1439`, `src/cli.rs:2204-2239`). Read stdout/stderr
   concurrently with hard caps and deterministic process-tree termination.

4. **Medium — The login prompt echoes tokens and reads input without a bound.**
   Secret input can appear on the terminal and an unbounded line can consume
   memory (`src/cli.rs:436-450`). Disable terminal echo for secret entry, cap
   input length, trim safely, and test both terminal and piped-input behavior.

5. **Medium — URL safety rules differ between `init` and server configuration.**
   An endpoint accepted in one path can be rejected or interpreted differently
   in another (`src/init.rs:56-88`, `src/server_config.rs:805-827`). Centralize a
   single URL validation policy with explicit context-specific exceptions and
   a shared test matrix.

6. **Medium — The manual hydrate/dehydrate verification script cannot exercise
   the workflow.** Its fixture is not initialized as a Git repository, while the
   CLI requires worktree registration (`scripts/manual/verify-local-cache-cli.sh:21-52`,
   `src/cli.rs:728-753`). Build a real temporary Git/LFS repository and make the
   script fail on unmet prerequisites instead of reporting misleading success.

7. **Low — Browser launch during login can block the CLI.** The browser command
   is invoked synchronously and may not return on some desktop integrations
   (`src/cli.rs:2169-2184`). Spawn it in a detached, bounded manner and always
   print the URL as a reliable fallback.

## Local cache

1. **High — Concurrent GC can delete the only preserved bytes during
   dehydration.** Dehydration publishes the cache object before replacing the
   worktree file, but it does not share GC's registry/operation lock, allowing GC
   to delete the new object in between (`src/local_cache.rs:692-701`,
   `src/local_cache.rs:852-881`). Coordinate GC and mutations through
   shared/exclusive operation locks or per-object pins held through pointer
   publication, and add a barrier-based race test.

2. **High — CLI cleanliness verification is tautological, accepts unrelated
   files, and preserves bytes only in the private cache.** The current bytes
   define the expected identity, so dirty edits, untracked files, outside paths,
   and non-LFS files all pass. A later `git lfs push` can then report the object
   missing, and losing the private cache loses the only preserved bytes
   (`src/cli.rs:665-690`, `src/cli.rs:743-760`, `src/cli.rs:1401-1406`,
   `src/cli.rs:1681-1772`, `src/cli.rs:4216-4267`,
   `IMPLEMENTATION.md:1161-1163`, `IMPLEMENTATION.md:1420`). Require a contained,
   Git-tracked `filter=lfs` path and derive expected identity from the index
   pointer. If intentional new-content ingestion is supported, make it an
   explicit mode that publishes into Git LFS media or upload state. Add a
   dehydrate-to-real-`git lfs push` test.

3. **Medium — Hydration and dehydration can overwrite a concurrent edit after
   their final check.** Each checks a path and then performs an unconditional
   replacing rename (`src/local_cache.rs:1457-1466`,
   `src/local_cache.rs:1723-1742`). Add per-path coordination and use conditional
   or exchange rename semantics where supported, retaining displaced data until
   its identity is verified. Add synchronized race tests.

4. **Medium — Temporarily unavailable worktrees are immediately pruned and
   their cache objects deleted.** `NotFound` is treated as permanent removal, so
   a disconnected volume or transient rename can cause data loss in the same GC
   run (`src/local_cache.rs:869-885`, `src/local_cache.rs:1025-1051`). Mark roots
   stale with a grace period or require explicit pruning; conservatively skip
   destructive collection when roots are unavailable.

5. **Medium — GC scans the raw filesystem instead of Git LFS tracked paths.** It
   recursively reads small files in ignored, generated, vendor, or dependency
   trees, and untracked pointer-shaped text can pin objects indefinitely
   (`src/local_cache.rs:1054-1155`). Enumerate NUL-safe tracked paths and evaluate
   `filter=lfs`; track any intentionally local-only dehydrated references
   explicitly.

6. **Medium — Worktree symlinks are followed and then replaced.** Dehydration
   can hash an outside-repository symlink target and replace the symlink with a
   pointer; hydration can similarly replace a symlink whose target is a pointer
   (`src/local_cache.rs:1336-1423`, `src/local_cache.rs:1556-1587`,
   `src/cli.rs:1681-1697`). Use `symlink_metadata`, no-follow opens where
   supported, canonical parent containment, and explicit symlink rejection.

7. **Medium — Dehydration performs three to four full reads of large files.**
   CLI hashing, library verification, cache copying, and the final race check
   each traverse the data (`src/cli.rs:1700-1772`,
   `src/local_cache.rs:671-701`, `src/local_cache.rs:1461`,
   `src/local_cache.rs:2032-2050`). Derive identity during a single staged copy,
   validate source metadata around it, and minimize final checks under the
   operation lock. Add read-count instrumentation or benchmarks.

8. **Medium — New materialized files bypass a restrictive process umask.** The
   implementation explicitly sets mode `0644`, potentially exposing private
   repository content to other local users (`src/local_cache.rs:30`,
   `src/local_cache.rs:1695-1701`, `src/local_cache.rs:2470-2489`). Respect the
   process umask or default to `0600`, then apply only appropriate Git-index mode
   bits. Test in a subprocess with umask `077`.

9. **Low — The worktree registry cannot represent valid non-UTF-8 Unix roots.**
   Direct JSON serialization of `PathBuf` rejects such paths
   (`src/local_cache.rs:232-244`, `src/local_cache.rs:978-1008`). Introduce a
   versioned platform-safe encoding, or explicitly reject and document the
   limitation with a targeted error and Unix-only test.

## Migration

1. **High — Current-checkout discovery misses hydrated and sparse LFS files.** It
   reads worktree files and only recognizes pointer placeholders, so hydrated
   content and paths absent from a sparse checkout are skipped
   (`src/migration.rs:394-418`, `src/cli.rs:918-925`,
   `tests/migration_fixture_repos.rs:23-50`). Read pointer blobs from the Git
   index for the current checkout and add hydrated/sparse fixtures.

2. **High — A migration dry run can lazy-fetch missing partial-clone objects.**
   `git cat-file` helpers inherit the environment, so supposedly read-only
   discovery can trigger network transfer (`src/migration.rs:2217-2267`,
   `src/migration.rs:2414-2452`). Set `GIT_NO_LAZY_FETCH=1`, detect promisor
   objects, and report unavailable data explicitly.

3. **High — Source and target repository identities can silently diverge.** The
   target comes from `origin`, current-checkout source selection can follow the
   current branch remote, and all-ref scans include every remote
   (`src/cli.rs:805-824`, `src/migration.rs:1323-1405`,
   `src/migration.rs:1921-1953`). Define an explicit source-remote contract,
   display it in the plan, and require confirmation or a flag for cross-remote
   migration.

4. **High — Shallow clones are treated as complete migration inventories.** The
   history scan does not reject or prominently qualify truncated history
   (`src/migration.rs:1956-2055`). Detect shallow repositories and block complete
   modes or require an explicit incomplete-history override with warnings and
   tests.

5. **High — Purge-manifest candidates are labeled complete without a verified
   migration receipt.** The CLI can present a destructive follow-up inventory
   based on planning rather than confirmed upload state (`src/cli.rs:1250-1305`,
   `src/cli.rs:3510-3566`, `README.md:247-249`). Generate purge input only from a
   durable, integrity-verified completion receipt and distinguish planned from
   uploaded objects.

6. **High — Migration access checks can report success without checking
   repository or storage access.** Target readiness is TCP-only and storage
   readiness can stop at credential parsing (`src/cli.rs:1009-1053`,
   `src/cli.rs:1097-1112`, `IMPLEMENTATION.md:498-501`). Rename these checks to
   their actual scope or add real read-only GitHub permission, server readiness,
   and Drive root probes.

7. **Medium — Dry-run upload counts ignore objects already present at the
   destination.** Planning reports every available source object as an upload,
   although execution skips existing targets (`src/cli.rs:1162-1232`,
   `src/migration.rs:646-650`). Perform bounded target existence checks and
   report new, existing, missing, and unknown counts separately.

8. **High — History discovery is O(commits × full tree) and launches many Git
   subprocesses.** Large repositories can make all-ref migration impractical
   (`src/migration.rs:1956-2267`). Batch object and attribute queries, stream
   results, cache across commits, and add representative scale benchmarks.

9. **High — Git command output limits are applied only after unbounded
   allocation.** Helpers use `Command::output` and truncate after capture,
   including tree enumeration (`src/migration.rs:2058-2082`,
   `src/migration.rs:2414-2509`). Pipe and drain stdout/stderr concurrently with
   hard byte caps, aborting and cleaning up the process tree on overflow.

10. **Medium — Default checkout migration gives no warning about objects on
    other refs.** The report can appear complete even though only the current
    checkout is inventoried (`src/cli.rs:898-926`, `src/cli.rs:1155-1247`,
    `IMPLEMENTATION.md:542`). State the scope prominently and report that other
    refs were not scanned.

11. **Medium — Migration silently requires Git 2.40 or newer.** Historical
    attribute discovery uses `git check-attr --source`, but installation docs do
    not declare or preflight the version (`src/migration.rs:1633-1651`,
    `README.md:9-13`). Add a version check, document the minimum, and return an
    actionable compatibility error.

12. **Medium — Source fetch scope can expand through
    `lfs.fetchrecentalways=true`.** Migration fetch commands do not override the
    setting, allowing unexpected extra downloads (`src/migration.rs:821-860`).
    Set the relevant Git LFS recent-fetch options explicitly and test hostile
    repository configuration.

13. **Medium — Required dry-run report fields are missing.** Reports omit
    tracked LFS patterns, Git LFS/filter readiness, quota or missing-object
    warnings, and byte totals (`src/cli.rs:1155-1247`). Add the fields defined by
    the implementation plan and make unknown values explicit.

14. **Medium — The manual migration-upload script exercises only fake stores.**
    It does not validate the live Drive upload path
    (`scripts/manual/verify-migration-upload.sh:9-16`). Add a gated disposable
    Drive scenario or rename and document the script as simulated verification.

15. **Medium — Large migration uploads are serialized and lose accumulated
    progress on failure.** One failed object aborts the run without a durable
    per-object result ledger (`src/migration.rs:639-665`). Add bounded
    concurrency, checkpoint completed objects, and return structured outcomes
    for retry.

16. **Low — Availability and upload paths repeatedly rehash the same object.**
    Both Git LFS media and shared-cache candidates can be hashed, followed by
    another upload verification pass (`src/migration.rs:558-573`,
    `src/migration.rs:653-656`). Short-circuit after a verified source and carry
    trusted verification metadata through the upload pipeline.

## Git LFS protocol and provider abstractions

1. **Medium — Batch parsing ignores `hash_algo` and accepts prefixed OIDs.** The
   protocol parser can accept identities outside the intended Git LFS SHA-256
   shape (`src/lfs.rs:128-146`, `src/lfs.rs:200-207`,
   `src/lfs.rs:507-520`, `src/lfs.rs:568-573`). Require the supported hash
   algorithm, accept only the canonical 64-hex OID representation, and add
   compatibility tests against the [Git LFS batch
   API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md).

2. **Medium — The empty canonical Git LFS pointer representation is wrong.** The
   pointer parser/rendering path does not match the specification's canonical
   empty representation (`src/lfs.rs:325-328`, `src/lfs.rs:401-413`,
   `src/local_cache.rs:1426-1444`). Correct parsing/rendering and add fixture
   round trips from the upstream pointer specification.

3. **Medium — Pointer detection uses a 64 KiB cutoff instead of Git LFS's 1,024
   byte limit.** Multiple call sites will treat larger pointer-shaped files as
   valid pointers (`src/lfs.rs:325-399`, `src/cli.rs:48`,
   `src/local_cache.rs:35`, `src/migration.rs:34-40`). Centralize the canonical
   limit and test the 1,024/1,025-byte boundary.

4. **Medium — Pointer parsing accepts non-canonical uppercase and whitespace.**
   Lenient OID/version parsing can diverge from Git LFS clients
   (`src/lfs.rs:128-162`, `src/lfs.rs:200-207`, `src/lfs.rs:416-423`,
   `src/lfs.rs:456-473`, `src/lfs.rs:980-990`, `src/lfs.rs:1065-1085`). Enforce
   canonical lowercase and line syntax, or explicitly separate tolerant input
   from canonical output with interoperability tests.

5. **Medium — Duplicate extension priorities are accepted.** Multiple
   extensions can claim the same order, making interpretation ambiguous
   (`src/lfs.rs:294-305`, `src/lfs.rs:344-382`). Reject duplicate priorities and
   add upstream-compatible pointer fixtures.

6. **Low — Historical Git LFS version aliases are rejected without an explicit
   compatibility decision.** The version parser accepts only the current URL
   (`src/lfs.rs:335-342`). Verify intended client compatibility, then either
   support documented historical aliases or document and test the rejection.

7. **High — Storage-provider abstractions and test fakes do not consistently
   enforce repository namespace.** The generic trait addresses objects only by
   OID/size, while integration and server fakes key objects only by LFS identity
   or ignore the repository argument, masking cross-repository isolation
   failures (`src/providers.rs:137-216`, `src/lib.rs:89-90`,
   `src/server.rs:198-231`, `src/server.rs:490-550`, `src/server.rs:2600`,
   `tests/support/mod.rs:364`). Include repository/storage namespace in the
   contract before adding providers or migration callers. Add two-repository
   contract tests sharing a provider and OID, proving upload and authorization
   for one repository do not expose the other.

8. **Medium — The repository-provider trait lacks authentication context and is
   bypassed by production authorization.** Its shape cannot express the
   per-session credential checks the server needs (`src/providers.rs:117-135`,
   `src/server.rs:770-847`). Redesign the abstraction around explicit actor/token
   context or remove the misleading unused boundary.

9. **Medium — Upload batch lookups are sequential while downloads are bounded
   concurrent.** Large upload batches incur avoidable provider latency
   (`src/server.rs:1656-1677`, `src/server.rs:1687-1707`). Use the same bounded
   concurrency and stable result ordering for both operations.

10. **Low — Provider tests are tautological and fake an inaccurate permission
    hierarchy.** Tests mostly restate mock behavior rather than enforce a
    production contract (`src/providers.rs:234-364`,
    `src/providers.rs:373-485`, `tests/support/mod.rs:29-230`). Replace them with
    contract tests shared by real and fake providers, including denial and
    repository-isolation cases.

## Test suite

1. **High — No test exercises Git LFS through the real HTTP boundary.** The
   nominal end-to-end test explicitly excludes Git LFS, uses `Router::oneshot`,
   and manually sends Bearer authentication; Basic authentication is tested only
   through a helper (`tests/local_end_to_end.rs:1`,
   `tests/local_end_to_end.rs:87`, `tests/local_end_to_end.rs:271-297`,
   `src/server.rs:2860`). Bind an ephemeral TCP listener, configure a temporary
   credential helper, and run real `git lfs push/fetch/checkout` commands.

2. **High — Tests bypass the production server composition path.** The tested
   router omits production configuration loading, SQLite synchronization, OAuth
   routes, Drive store construction, listener binding, and shutdown
   (`src/server.rs:93`, `src/server.rs:198`, `tests/local_end_to_end.rs:79`).
   Refactor assembly into an injectable server builder and exercise the complete
   production composition.

3. **High — Live-provider tests stop before object transfer.** GitHub coverage
   checks identity and permission, while Drive coverage checks only root-folder
   validation (`tests/external_integrations.rs:29`,
   `tests/external_integrations.rs:98`). Add a disposable live scenario covering
   server upload, Drive properties, SQLite metadata, action download, integrity,
   and cleanup.

4. **Medium — Redaction tests do not inspect emitted tracing events.** Server
   failure paths interpolate errors into tracing fields, but tests inspect only
   values and error strings (`src/server.rs:902`, `src/server.rs:976`). Capture
   tracing output and assert sentinel OAuth, credential, Drive, URL, and helper
   secrets never appear.

5. **Medium — Missing prerequisites are reported as successful tests.** Many CLI
   tests silently return without Git, and explicitly selected gated tests can
   also return without their enable flag or Git LFS
   (`src/cli.rs:3026`, `tests/external_integrations.rs:27`,
   `src/migration.rs:4016`). Make CI prerequisites fail clearly; keep external
   tests ignored by default but fail an explicitly requested run that lacks its
   required tool or flag.

6. **Medium — macOS copy-on-write tests accept ordinary copying.** The assertions
   allow either result even on environments intended to protect the CoW feature
   (`src/local_cache.rs:2440`, `src/cli.rs:4173`). Assert CoW on supported APFS
   fixtures and test fallback copying separately on unsupported filesystems.

7. **Medium — Platform-specific process handling is not tested across supported
   operating systems.** Windows timeout cleanup uses `taskkill`, while relevant
   helper and timeout tests are Unix-only (`src/credentials.rs:970`,
   `src/migration.rs:1019`, `src/credentials.rs:1313`). Run CI on Linux, macOS,
   and Windows with platform-native fake helpers and process-tree cases.

8. **Low — A project-shape test enforces a redundant dependency roster.** It
   requires every originally planned dependency even if unused
   (`tests/project_shape.rs:96`). Replace it with actionable policy checks such
   as forbidden TLS backends, unwanted default features, advisory scanning, and
   unused dependency detection.

9. **Low — Hostile-input parsers lack generative robustness coverage.** Batch
   rejection and other parsers rely on finite example tables
   (`src/lfs.rs:1434`). Add fuzz/property targets for panics, bounded work,
   oversized/deep inputs, parse/render round trips, and redaction invariants.

10. **Low — Test-only unsafe environment mutation has an invalid safety
    rationale.** The logging tests mutate process-global environment state under
    assumptions that are not enforced across the entire test process
    (`src/logging.rs:212`, `src/logging.rs:222`, `src/logging.rs:237`). Move
    environment-sensitive cases to subprocess tests or globally serialize them
    with an enforceable mechanism.

## Release readiness

1. **High — The repository's licensing state blocks public release.** Both Rust
   and JavaScript package metadata declare the project unlicensed/private
   (`Cargo.toml:6`, `package.json:6-7`). Choose and add a license, update package
   metadata, and confirm third-party license compatibility before publication.

2. **Medium — CI lacks an explicit dependency-advisory gate.** The repository
   does not enforce ongoing vulnerability review, and `cargo audit` is not
   available in the documented workflow (`README.md:67`). Add a pinned
   `cargo-audit` or equivalent supply-chain job, define update ownership, and
   fail on applicable advisories. Track relevant RustSec advisories, including
   [RUSTSEC-2026-0048](https://rustsec.org/advisories/RUSTSEC-2026-0048.html),
   [RUSTSEC-2026-0049](https://rustsec.org/advisories/RUSTSEC-2026-0049.html),
   and
   [RUSTSEC-2025-0047](https://rustsec.org/advisories/RUSTSEC-2025-0047.html).
