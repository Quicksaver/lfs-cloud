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

13. **[DONE] Session revocation had no user-facing route or CLI flow** (Medium,
    `src/server.rs`, `src/sessions.rs`, `src/credentials.rs`, and `src/cli.rs`):
    **Valid and actionable.** The session store already supported in-memory and
    durable token revocation, but production exposed no authenticated endpoint
    or CLI command that could invoke it, and a definitively invalid private
    GitHub token left its local bearer session active. The full server now
    mounts authenticated `DELETE /auth/session`, consumes the presented local
    token, and removes both its in-process record and durable SQLite row.
    `lfs-cloud logout --server` resolves the current repository route, performs
    a non-redirecting authenticated revocation request, and only then erases the
    exact path-scoped credential through `git credential reject`; an already
    expired or revoked server token still permits local cleanup, while an
    unexpected server failure preserves the credential for retry. LFS batch,
    upload, and download authorization now carry the authenticated local token
    through the request boundary and revoke it when GitHub definitively returns
    `AuthenticationRequired`. Coverage proves endpoint replay denial, automatic
    upstream-invalid revocation, durable deletion across database reopen,
    repository-context credential rejection, CLI ordering, stale-credential
    cleanup, redacted output, and command dispatch. README, implementation
    guidance, the repository learning, and
    `scripts/manual/verify-logout-command.sh` document and exercise the new
    contract. Verification passed with `cargo fmt --check`, `yarn lint:fix`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`,
    `scripts/manual/verify-logout-command.sh`, and `git diff --check`. The
    reviewer's core finding and all three remediation requirements were precise
    and security-relevant. Its suggestion that restart ended exposure was stale
    because production sessions now survive restarts, but that makes explicit
    durable revocation more important rather than invalidating the finding;
    with one valid finding and no invalid finding attributable separately, this
    was high-quality feedback with one minor outdated detail.

14. **[DONE] Authentication performs a serialized O(n) session scan and clones
    OAuth-bearing records** (Low, `src/sessions.rs` and `src/server.rs`):
    **Valid and actionable.** Session authentication previously retained every
    map entry while holding the mutex, then deep-cloned the matching record,
    including its private GitHub OAuth token. Verification now hashes the
    presented token before taking the lock, performs one direct `BTreeMap`
    entry lookup, removes only that entry when it has expired, and returns an
    `Arc` to the stored OAuth-bearing record. The authenticated server request
    carries that same shared record, and revocation likewise removes only the
    presented token instead of scanning unrelated sessions. Full-map expiry
    pruning remains only on session admission and diagnostic length paths,
    where bounded capacity must be reconciled and which are not part of the
    per-request authentication hot path; an expiry index or background task
    would therefore add lifecycle complexity without improving normal request
    lookup. A deterministic regression proves that validating an active token
    leaves an unrelated expired entry untouched, returns the exact stored
    `Arc`, and removes the expired entry only when that token is itself checked.
    Verification passed with `yarn lint:fix`, `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`.
    The focused reviewer found a genuine low-severity scalability and
    secret-handling inefficiency, identified both sources of avoidable work,
    and recommended the decisive exact-entry and shared-record changes. With
    one valid finding assessed here and no invalid finding attributable
    separately, this was high-quality, relevant feedback.

15. **[DONE] GitHub REST requests did not pin an API version** (Low,
    `src/github_auth.rs`): **Valid and actionable.** Authenticated-user lookup,
    repository identity verification, and collaborator permission checks all
    set GitHub's recommended `Accept` media type but omitted
    `X-GitHub-Api-Version`, leaving response semantics tied to GitHub's moving
    unversioned default. GitHub's official
    [API-version documentation](https://docs.github.com/en/rest/about-the-rest-api/api-versions)
    currently lists `2022-11-28` as supported through March 10, 2028; pinning
    that version preserves the behavior the unversioned requests already
    received while avoiding an unreviewed move to the newer breaking API
    version. A single
    `github_api_request` helper now applies both the media type and version
    headers to every GitHub REST request, including requests made with injected
    clients. The mocked user and permission servers capture and assert the
    exact version header for user lookup, repository identity lookup, and
    collaborator permission lookup. The new assertions failed before the
    implementation and passed afterward. Verification passed with `cargo fmt`,
    `yarn lint:fix`, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets -- --test-threads=1`, and `cargo test --doc`. An
    earlier default-parallel run overlapped a detached invocation of the same
    suite and produced credential-helper timeouts; after stopping only those
    two overlapping test process groups, the isolated serial suite passed with
    512 unit tests, all integration targets, and the expected ignored external
    tests. The focused reviewer identified a genuine low-severity forward-
    compatibility risk and prescribed the precise centralized-header and mock-
    assertion remediation; with one valid finding assessed here and no invalid
    finding attributable separately, this was high-quality, relevant feedback.

16. **[DONE] Credential-helper descendants could leave blocked pipe-reader
    threads** (Low, `src/credentials.rs`): **Valid and actionable.** Timeout
    handling already attempted descendant cleanup, but a successful direct Git
    child could exit while a helper descendant retained stdout or stderr. The
    bounded drain then returned while dropping the pipe readers' join handles,
    leaving their threads detached and blocked until the descendant eventually
    closed the inherited descriptors. Credential Git commands now enter a
    dedicated Unix process group, and both successful-exit and timeout paths
    terminate remaining process-tree members before finishing an open pipe.
    Successful-exit handling drains buffered output, confirms EOF, and joins
    every reader thread before returning; Windows continues to use recursive
    `taskkill /T` cleanup. A Unix regression starts a successful fake Git child
    whose sleeping descendant inherits both output pipes, verifies the direct
    child's output is preserved, and proves the descendant is gone when the
    wait returns. The new test failed against the previous implementation and
    passed after the fix. Verification passed with `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`,
    `yarn lint:fix`, and `git diff --check`. The focused reviewer identified a
    genuine low-severity process-lifecycle leak, distinguished direct-child
    completion from inherited-pipe EOF correctly, and requested the decisive
    pipe-holding-descendant regression. With one valid finding assessed here
    and no invalid finding attributable separately, this was high-quality,
    relevant feedback.

## Server and metadata

1. **[DONE] Unbounded batches and per-object authorization can amplify into
   thousands of GitHub, OAuth, and Drive API calls** (High, `src/server.rs`,
   `src/server_config.rs`, and server configuration documentation): **Valid and
   actionable.** Batch requests now accept at most the configured
   `server.max_batch_objects` entries (100 by default); duplicate entries count
   toward that request/response bound, but storage lookups are collapsed by
   exact OID and size while duplicate response entries and ordering are
   preserved. Unsupported transfers, oversized upload objects, and malformed
   transfer size queries are rejected before GitHub authorization or storage
   work. One process-wide semaphore, configured by
   `server.max_provider_calls` (16 by default), bounds repository and storage
   provider calls across every repository. Successful permission decisions are
   cached for only 15 seconds and keyed by the exact local session token,
   repository ID, and read/write operation; per-key locking single-flights
   concurrent misses, so a batch action can reuse its immediately preceding
   authorization without widening access. Google Drive access tokens are
   cached only until 60 seconds before their reported expiry, and concurrent
   refresh misses collapse into one OAuth request. Regression coverage proves
   over-limit and unsupported batches make no provider calls, duplicate entries
   perform one lookup, the global provider-call peak respects configuration,
   batch-to-transfer authorization performs one permission check, malformed
   actions do not authorize, invalid zero limits are rejected, and concurrent
   Drive token requests refresh once. README, implementation notes, full config
   documentation, and the repository learning now describe these boundaries.
   Verification passed with `yarn lint:fix`, `cargo fmt --all --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`, and
   `git diff --check`. The focused reviewer identified a genuine high-severity
   cross-provider amplification path, traced each compounding call boundary,
   and prescribed the complete layered remediation plus call-count tests. With
   one valid finding assessed here and no invalid finding attributable
   separately, this was high-quality, security- and availability-relevant
   feedback.

2. **[DONE] Authenticated batch bodies had no read timeout or global request
   limit** (High, `src/server.rs`, `src/server_config.rs`, and server
   configuration documentation): **Valid and actionable.** Authentication
   correctly happened before body buffering, but the authenticated
   `Bytes::from_request` future could wait forever while a valid client dripped
   bytes, and every such body had an independent unbounded request slot. Batch
   bodies now use an explicit bounded reader that preserves the existing 2 MiB
   size limit while enforcing a 15-second inter-chunk idle timeout and a
   60-second end-to-end deadline; timeout failures return protocol-compatible
   Git LFS JSON with HTTP 408. A process-wide middleware semaphore admits at
   most `server.max_concurrent_requests` active HTTP requests across OAuth,
   session, batch, and transfer routes (64 by default), rejecting excess work
   immediately with HTTP 503 and `Retry-After: 1` rather than creating an
   unbounded waiter queue. Request admission is the decisive boundary for this
   finding because a slow batch body is already an active HTTP request; a
   separate pre-request TCP connection cap would not replace these body
   deadlines and can remain a listener or trusted reverse-proxy control.
   Router-level regressions prove an idle authenticated body times out, a
   continuously dripping body cannot evade the total deadline, and a second
   request is rejected while the sole configured slot is occupied. Config
   parsing rejects a zero limit, and README, implementation, configuration,
   and repository-learning documentation describe the operational contract.
   Verification passed with `cargo fmt --all`, `yarn lint:fix`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`, and
   `git diff --check`. The focused reviewer identified a genuine high-severity
   authenticated slow-body availability flaw, correctly required independent
   idle and total bounds so byte dripping could not reset the only deadline,
   and requested the decisive router-level admission tests. With one valid
   finding assessed here and no invalid finding attributable separately, this
   was high-quality, security- and availability-relevant feedback.

3. **[DONE] Upload free-space checks race and do not reserve aggregate
   capacity** (High, `src/server.rs`, `src/server_config.rs`, and server
   configuration documentation): **Valid and actionable.** Upload staging
   previously checked each request against an independent temp-filesystem
   free-space snapshot, so concurrent uploads could all pass while their
   aggregate declared sizes exceeded available capacity. Staging admission now
   has configurable process-wide and per-user concurrency limits, defaulting to
   eight and two respectively. Stable repository-provider user IDs define the
   per-user boundary when available, and overload is rejected immediately with
   HTTP 503 plus `Retry-After: 1`. Each admitted upload atomically reserves its
   declared byte weight under a shared lock before reading the body. The
   concurrency permits and byte reservation live in the staged-upload lease,
   which remains attached to the temporary file through backend storage and is
   released on every success, error, or drop path. The live filesystem check,
   64 MiB headroom, upload-size limit, idle body timeout, and write-time
   ENOSPC/quota classification remain independent secondary guardrails.
   Configuration parsing rejects zero limits and per-user limits above the
   process-wide limit. Deterministic coverage proves global and per-user slot
   enforcement, aggregate weighted admission and release, simultaneous
   reservation atomicity, overload response shape, and configuration defaults
   and validation. README, implementation, configuration, and repository
   learning documentation now describe the staging-capacity contract.
   Verification passed with `cargo fmt --all --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`,
   `yarn lint:fix`, and `git diff --check`. The focused reviewer identified a
   genuine high-severity aggregate resource-exhaustion race, correctly
   distinguished a free-space observation from a reservation, and prescribed
   both weighted capacity accounting and fairness limits plus the secondary
   filesystem defense. With one valid finding assessed here and no invalid
   finding attributable separately, this was high-quality, security- and
   availability-relevant feedback.

4. **[DONE] Startup does not validate Drive credentials or root-folder
   usability** (Medium, `src/server.rs`, `README.md`, `IMPLEMENTATION.md`, and
   `docs/configuration.md`): **Valid and actionable.** Production previously
   constructed the Drive transfer store and bound the listener without loading
   storage credentials, refreshing an access token, or invoking the existing
   root validator; the first batch lookup therefore discovered an unusable
   provider after the server had already reported readiness. `serve` now
   validates every configured Drive provider before router construction and
   listener binding. The startup gate loads each configured credential,
   refreshes it through Google OAuth, and performs the non-mutating Drive
   metadata probe that requires a live folder with child-write capability. A
   missing/invalid credential, token-refresh failure, missing/non-folder root,
   or read-only root now aborts startup as a typed storage error. Startup and
   transfer paths share the access-token cache, avoiding an immediate second
   OAuth refresh after readiness validation. Dependency-injected regressions
   prove the complete refresh-plus-root-probe composition, one refresh reused by
   the first transfer-store construction, rejection of a read-only root, and
   access-token redaction from the failure. README, implementation,
   configuration, server API, and repository-learning documentation now state
   the fail-closed startup contract. Verification passed with `yarn lint:fix`,
   `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, focused startup tests, all integration test targets,
   `cargo test --doc`, and `git diff --check`. The serial all-target run passed
   527 tests with one ignored but hit three unrelated load-sensitive
   credential-helper timeouts; all 35 credential tests, including those three,
   passed together immediately on retry. The focused reviewer identified a
   genuine medium-severity readiness flaw, connected it to both credential and
   root usability, and prescribed the correct fail-closed startup boundary.
   With one valid finding assessed here and no invalid finding attributable
   separately, this was high-quality, operationally relevant feedback.

5. **[DONE] Per-object upload locks leak permanently** (Medium,
   `src/server.rs`): **Valid and actionable.** The upload-lock map previously
   stored a strong `Arc` for every repository/storage-provider/OID key, so the
   mutex allocation and key survived after the last holder and waiter had
   completed. The map now stores `Weak` mutex references, purges dead entries on
   every lock admission, reuses a live mutex when one can be upgraded, and
   creates a replacement only after no holder or waiter can still own the old
   mutex. Upgrade and insertion remain under the map mutex, so concurrent
   requests cannot split same-object serialization across two live locks. The
   test-first lifecycle regression failed against the strong-reference map and
   now proves that a completed upload retains no lock allocation and that the
   next distinct upload removes its stale key. The existing concurrent-retry
   regression continues to prove that two live uploads for the same object
   serialize and perform only one backend write. The repository learning now
   records the weak single-flight lock contract. Verification passed with
   `cargo fmt --all`, `yarn lint:fix`, `git diff --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, and `cargo test --doc`. The focused reviewer
   identified a genuine medium-severity, attacker-amplifiable memory-retention
   flaw, pinpointed the ownership mistake, and recommended both an appropriate
   weak/bounded lock-manager design and the decisive lifecycle regression; with
   one valid finding assessed here and no invalid finding attributable
   separately, this was high-quality, relevant feedback.

6. **[DONE] Graceful shutdown and transfer draining were absent** (Medium,
   `src/server.rs`, `README.md`, `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid
   and actionable.** Production previously awaited `axum::serve` directly, so
   normal process termination provided no listener shutdown or in-flight
   transfer drain boundary. The server now handles SIGINT and SIGTERM on Unix
   (and Ctrl+C on other supported targets), passes the signal to Axum's
   graceful-shutdown path to stop new listener admission, and waits up to a
   documented 30 seconds for active batch and object-transfer requests. The
   deadline is applied outside Axum's otherwise unbounded graceful wait; after
   it expires, `serve` returns so process shutdown terminates remaining work and
   content-addressed clients can retry. Test-first TCP regressions prove that a
   request already in flight completes while new connections are refused, and
   that a permanently blocked request cannot retain the server past its drain
   deadline. README and implementation guidance document the operational
   contract, while the repository learning preserves the deadline-placement
   rationale. Verification passed with `cargo fmt --all`, `yarn lint:fix`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
   focused reviewer identified a genuine medium-severity shutdown-integrity and
   availability flaw, recommended Axum's correct graceful-shutdown mechanism,
   and included the essential bounded-drain requirement; with one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, operationally relevant feedback.

7. **[DONE] Metadata config synchronization retained stale routes and could
   block legitimate renames** (Medium, `src/metadata.rs`, `IMPLEMENTATION.md`,
   and `AGENTS.md`): **Valid and actionable.** A test-first replacement-mapping
   regression reproduced the startup failure as
   `UNIQUE constraint failed: repository_mappings.route_path`: synchronization
   upserted only current IDs, so a removed mapping continued to reserve its
   route indefinitely. Metadata schema version 4 now adds an `is_active`
   marker. In the existing configuration-sync transaction, removed mappings and
   mappings whose route changed are first marked inactive and assigned a
   non-routable `inactive:<mapping-id>` tombstone; current mappings are then
   upserted as active with their configured routes. The inactive parent rows
   remain in place, preserving object and transfer-attempt foreign-key history
   instead of deleting it through cascades, while the unique public route can
   be reused safely. Upgrade coverage proves version-3 mappings become active,
   and the replacement regression proves a new mapping can claim a removed
   mapping's route without losing the original mapping's verified object row.
   Implementation guidance and the repository learning now document the
   reconciliation contract. Verification passed with `yarn lint:fix`,
   `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test --all-targets`, `cargo test --doc`, and
   `git diff --check`. The focused reviewer identified a genuine
   medium-severity startup availability and configuration-lifecycle flaw,
   correctly connected the unique route to stale persisted state, and proposed
   the right transactional active-marker design while preserving history; with
   one valid finding assessed here and no invalid finding attributable
   separately, this was high-quality, operationally relevant feedback.

8. **[DONE] Idempotent verification rewrites original uploader attribution**
   (Medium, `src/metadata.rs`, `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid
   and actionable.** The object upsert preserved `created_at` but replaced all
   three `created_by` columns with the user performing each later verification,
   so an idempotent upload or stale-row repair silently rewrote provenance.
   Conflict updates now refresh only the backend ID, verification status, and
   `last_verified_at` timestamp; the original creator fields remain immutable.
   The insert parameter and public record documentation clarify that creator
   attribution applies only when the row is first created. Regression coverage
   proves that both a duplicate upload by a different stable user and repair of
   a stale row preserve the first recorded creator while updating backend and
   verification metadata. Implementation guidance and the repository learning
   now document the provenance contract. Verification passed with `cargo fmt`,
   `yarn lint:fix`, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, and `cargo test --doc`; the final diff check also
   passed. The focused reviewer identified a genuine medium-severity audit
   integrity flaw, correctly distinguished immutable creator provenance from
   mutable verification metadata, and requested the decisive duplicate-update
   behavior; with one valid finding assessed here and no invalid finding
   attributable separately, this was high-quality, relevant feedback.

9. **[DONE] Newer metadata schemas were silently accepted and could be
   modified** (Medium, `src/metadata.rs`, `src/error.rs`,
   `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid and actionable.** The
   migration runner previously executed its initial schema batch before reading
   `PRAGMA user_version`, and a version above the
   binary's supported version skipped every known migration without producing
   an error. It now reads the version before starting a transaction or
   executing schema SQL and returns a typed error containing the database path,
   found version, and supported version when the schema is newer. A regression
   creates a version-5 database with an unknown table and sentinel row, proves
   opening it fails, and then reopens it directly to prove the version, table
   set, and data remain unchanged. Implementation guidance and the repository
   learning now document the forward-schema guard. Verification passed with
   `cargo fmt`, `yarn lint:fix`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
   focused reviewer identified a genuine medium-severity forward-compatibility
   and data-integrity flaw, pinpointed the unsafe operation ordering, and
   requested the decisive non-mutation regression. With one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, relevant feedback.

10. **[DONE] Synchronous SQLite and a standard mutex block async request
    workers** (Medium, `src/metadata.rs`, `src/server.rs`, `src/error.rs`,
    `src/github_auth.rs`, `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid and
    actionable.** Production upload completion awaited an async transfer-store
    future that directly acquired the standard metadata connection mutex and
    performed a synchronous SQLite upsert. Contention or SQLite's five-second
    busy wait could therefore stop a Tokio request worker. Verified-object
    recording now crosses an owned-input async boundary that runs the complete
    mutex acquisition and SQLite operation through `spawn_blocking`; the MVP
    retains one serialized connection without blocking async workers. Blocking
    task join failures have a typed server error and remain generic in OAuth
    responses. Startup migrations and configuration reconciliation stay
    synchronous because they finish before the listener admits requests. A
    single-thread-runtime regression holds an external exclusive SQLite lock,
    proves a Tokio timer still advances while the metadata write waits, then
    releases the lock and verifies the write succeeds. Implementation guidance
    and the repository learning document the request-path boundary.
    Verification passed with `cargo fmt`, `yarn lint:fix`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`.
    The focused reviewer identified a genuine medium-severity async-runtime
    starvation risk, traced it to the precise request-path database write, and
    proposed the appropriate blocking-task alternative for the current
    single-connection MVP. With one valid finding assessed here and no invalid
    finding attributable separately, this was high-quality, relevant feedback.

11. **[DONE] Size-only integrity failures incorrectly report a SHA-256
    mismatch** (Low, `src/server.rs`): **Valid and actionable.** Upload staging
    correctly represented both OID and size failures with the existing
    `StorageError::IntegrityMismatch`, but the shared HTTP mapping described
    every mismatch as a bad SHA-256. The Git LFS error response now states that
    the uploaded object did not match the requested OID or size, accurately
    covering both integrity dimensions without adding redundant error variants.
    The existing route-OID and batch-size endpoint regressions both assert the
    corrected client-facing contract; the size case uses bytes whose SHA-256 is
    exactly the route OID, proving that a size-only failure no longer receives a
    hash-only diagnostic. Verification passed with `yarn lint:fix`, `cargo fmt`,
    focused upload-integrity endpoint tests,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets`, and `cargo test --doc`. The focused reviewer
    identified a genuine, narrowly scoped client-diagnostic defect and
    suggested a proportionate generic response; with one valid finding assessed
    here and no invalid finding attributable separately, this was high-quality,
    relevant feedback.

12. **[DONE] Transfer-attempt metadata was declared but never recorded** (Low,
    `src/metadata.rs`, `src/server.rs`, and `AGENTS.md`): **Valid and
    actionable.** Production now passes the shared metadata database into LFS
    server state and creates one durable lifecycle row for each authenticated,
    structurally valid, repository-authorized upload or download. Attempts
    start before upload serialization, storage lookup, staging, or backend I/O;
    ordinary completion closes them exactly once as `succeeded` or `failed`.
    Successful rows retain the verified backend ID, while failed rows retain
    only the responsible error category and the same fixed, secret-free
    diagnostic safe for the Git LFS client. Raw provider errors, session
    tokens, and backend IDs from failed transfers never cross the metadata
    boundary. SQLite start and finish operations run through Tokio's blocking
    pool, preserving the existing async-worker boundary. A `started` row is
    intentionally left only when the process or request is interrupted or the
    terminal metadata write itself fails; download success means the verified
    backend response was prepared, not that the remote client consumed every
    response byte. Metadata tests cover started, successful, and sanitized
    failed rows, while an endpoint regression exercises a successful upload and
    failed-integrity download through the real request handlers and inspects the
    resulting SQLite history. The repository learning records the lifecycle
    boundary. Verification passed with `yarn lint:fix`, `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets` (540 passed, 3 ignored across targets),
    `cargo test --doc` (38 passed), and `git diff --check`. The focused reviewer
    found a genuine implementation-versus-schema gap, cited the exact unused
    persistence surface, and suggested both valid resolutions without inflating
    the severity. With one valid finding assessed here and no invalid finding
    attributable separately, this was high-quality, relevant feedback.

13. **[DONE] Public server documentation and the base-route error are stale**
    (Low, `src/server.rs`): **Partly stale, with one valid and actionable
    behavior defect.** The module and `serve` API documentation already state
    that the server proxies authenticated batch and object transfers, so that
    portion of the review had been addressed by earlier work and required no
    additional documentation change. The authenticated repository base path
    still returned HTTP 501 with the obsolete claim that transfer handling was
    not implemented. It now returns a Git LFS JSON HTTP 404 explaining that the
    base path is not an operation endpoint and directing clients to
    `/objects/batch`; the public `LfsRouteEndpoint::Info` documentation now
    makes the same distinction. A test-first regression failed against the old
    501 response and passes with the endpoint-specific status and message.
    Verification passed with `yarn lint:fix`, `cargo fmt --all`,
    `cargo clippy --all-targets -- -D warnings`, `cargo build`,
    `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`, and
    `git diff --check`. The reviewer found a genuine low-severity stale-response
    defect, but its public documentation claim was stale against the current
    tree. With one valid behavior issue and one already-addressed documentation
    assertion, this was useful and relevant but only moderately precise
    feedback.

## Google Drive storage

1. **[DONE] Concurrent or retried uploads can create duplicate Drive files and
   make an object permanently unreadable** (High, `src/metadata.rs`,
   `src/server.rs`, `src/google_drive.rs`, and storage configuration
   documentation): **Valid and actionable.** The final existence check and
   backend write were protected only by a server-state-local mutex, so two
   processes sharing the production metadata database could both observe a
   missing object and create separate Drive files. The upload handler now
   acquires a repository/storage/OID/size-keyed OS file lock rooted in the
   shared metadata directory before its final lookup and retains it through the
   Drive write and metadata record. Lock acquisition runs on Tokio's blocking
   pool, a fixed stripe set bounds persistent lock files, and closing the file
   releases the lock after normal completion or process exit. The documented
   MVP contract requires every local process writing one Drive root to share
   the metadata path; cross-host multi-writer operation remains out of scope.
   Existing duplicates no longer make objects unreadable: lookup verifies every
   returned exact match and deterministically selects the lexicographically
   smallest Drive file ID. A test-first regression with two independent server
   states and separate metadata connections to the same database failed before
   the fix and now proves that concurrent retries perform one backend upload;
   Drive lookup coverage proves reverse-ordered duplicates resolve to the same
   canonical ID. README, implementation, configuration, and repository-learning
   documentation record the single-writer and duplicate-recovery contracts.
   Verification passed with `yarn lint:fix`, `cargo fmt --all --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets` (559 passed, 3 ignored across targets),
   `cargo test --doc` (38 passed), and `git diff --check`. The focused reviewer
   identified a genuine high-severity idempotency and availability flaw,
   correctly distinguished process-local serialization from a durable boundary,
   and requested the decisive independent-state and retry coverage. With one
   valid finding assessed here and no invalid finding attributable separately,
   this was high-quality, security- and availability-relevant feedback.

2. **[DONE] Downloads described as streaming were fully staged before the first
   response byte** (High, `src/google_drive.rs`, `README.md`, and
   `IMPLEMENTATION.md`): **Valid and actionable.** The HTTP response path now
   proxies Drive chunks directly with constant-memory hashing instead of first
   copying every object into an unbounded local tempfile. Drive metadata and
   `Content-Length` are still validated before headers are returned; the body
   stream rejects excess, truncated, interrupted, or SHA-256-mismatched content
   and drops the upstream response immediately when the client disconnects.
   Destination-path downloads retain verified tempfile publication because
   their API contract requires atomic local publication. Existing focused tests
   cover corrupt and truncated streams, and documentation now states the exact
   end-of-stream verification boundary. Disk-exhaustion, staging quota, and
   tempfile-cleanup cases became inapplicable to HTTP proxy downloads because
   that path no longer writes local files. Verification passed with the full
   repository checks. The reviewer identified a genuine high-severity
   resource-exhaustion and latency flaw and offered both viable architectural
   remedies. With one valid finding and no invalid finding attributable
   separately, this was high-quality, relevant feedback.

3. **[DONE] Drive upload and download clients lack connect and per-read idle
   timeouts** (High, `src/google_drive.rs`, `README.md`,
   `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid and actionable.** Default
   Google provider clients now bound connection establishment to 10 seconds.
   Large object transfers intentionally retain no total request deadline, but
   enforce a 30-second progress deadline: Drive downloads use Reqwest's
   resettable per-read timeout, while uploads use a body-aware watchdog that
   resets as Reqwest consumes each staged-file chunk and remains active while
   awaiting response headers. Upload response bodies also apply the same
   per-chunk idle bound, and the small resumable-session initiation request
   retains its separate metadata deadline. A client-level upload
   `read_timeout` was deliberately avoided because Reqwest starts that timer
   before response headers, which would turn a healthy long upload into a
   time-to-first-response failure rather than measuring upload progress.
   Regression coverage proves that stalled upload responses and download
   streams terminate promptly, default transfer clients preserve the intended
   idle-versus-total timeout contract, and existing truncated-stream behavior
   remains intact. README and implementation guidance document the operational
   limits, while the repository learning records the non-obvious Reqwest
   boundary. Verification passed with `yarn lint:fix`, the all-crate Cargo
   format check, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1` (562 passed, 3 ignored across
   targets), `cargo test --doc`, and `git diff --check`. The focused reviewer
   identified a genuine high-severity indefinite-stall path and correctly
   required connect and resettable idle boundaries while warning against a
   total deadline for large transfers. With one valid finding assessed here
   and no invalid finding attributable separately, this was high-quality,
   security- and availability-relevant feedback.

4. **[DONE] Valid repository IDs can exceed Drive app-property limits**
   (Medium, `src/google_drive.rs`, `IMPLEMENTATION.md`, and `AGENTS.md`):
   **Valid and actionable.** Google Drive's current custom-property guidance
   confirms that each UTF-8 property string is limited to 124 bytes across its
   key and value, while repository mapping IDs have no corresponding length
   ceiling. Repository namespace metadata now preserves the raw namespace for
   backward compatibility only when the complete property fits that byte
   limit. An oversized namespace is represented by its fixed 64-character
   SHA-256 digest plus a separate `sha256` format property. The explicit format
   marker prevents a short raw namespace that resembles a digest from aliasing
   an oversized repository namespace. Upload metadata, lookup queries, and
   response verification all derive the same bounded property set. Boundary
   coverage proves that a namespace at exactly 124 combined bytes remains raw,
   the first oversized byte switches to tagged digest metadata, every emitted
   key/value pair remains within the provider limit, and both upload and lookup
   paths use the digest. Implementation guidance and the repository learning
   record the compatibility and isolation rule. Verification passed with
   `cargo fmt`, `yarn lint:fix`, the 61-test Google Drive module suite,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
   focused reviewer identified a genuine medium-severity provider-boundary
   failure, cited the authoritative byte limit, and requested both an
   appropriate bounded representation and decisive maximum-length tests. With
   one valid finding assessed here and no invalid finding attributable
   separately, this was high-quality, relevant feedback. See the [Drive
   custom-properties
   limits](https://developers.google.com/workspace/drive/api/guides/properties).

5. **[DONE] Paginated Drive lookup results were mishandled as conflicts**
   (Medium, `src/google_drive.rs`, `IMPLEMENTATION.md`, and `AGENTS.md`):
   **Valid and actionable.** Object lookup previously treated any
   `nextPageToken` as a storage conflict, never sent `pageToken`, and therefore
   could not discover or validate exact matches beyond the first Drive list
   page. Lookup now follows every opaque page token while retaining the exact
   repository/OID/size query, validates every returned candidate, and only
   after the final page decides absence or selects the lexicographically
   smallest verified Drive file ID. Blank tokens are rejected as malformed
   upstream responses, while repeated tokens produce a retryable failure so a
   bad provider response cannot loop forever. A mocked two-page regression
   proves that the second request carries `pageToken`, preserves the lookup
   query, and reconciles duplicates split across pages to the same canonical
   file ID. Implementation guidance and the repository learning record the
   pagination and duplicate-recovery boundary. Verification passed with
   `yarn lint:fix`, `cargo fmt --all --check`, the 62-test Google Drive module
   suite, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1` (565 passed, 3 ignored across
   targets), `cargo test --doc` (38 passed), and `git diff --check`. The focused
   reviewer identified a genuine medium-severity availability and correctness
   flaw, precisely located both the missing request token and the premature
   conflict decision, and requested the decisive all-pages behavior. With one
   valid finding assessed here and no invalid finding attributable separately,
   this was high-quality, relevant feedback.

6. **[DONE] The resumable upload implementation did not actually resume**
   (Medium, `src/google_drive.rs`, `README.md`, `IMPLEMENTATION.md`, and
   `AGENTS.md`): **Valid and actionable.** The upload path previously opened a
   resumable session but sent the complete staged file in one request, mapped
   HTTP 308 to a retryable error, and required the outer operation to create a
   new session. It now sends the verified file in 256 KiB-aligned chunks except
   for the final chunk, includes exact `Content-Range` metadata, and treats
   Drive's HTTP 308 `Range` response as the authoritative committed offset.
   Transport and retryable provider failures trigger an empty status `PUT` to
   the same session; a completed probe returns the verified Drive object, while
   an incomplete probe seeks the staged file to Drive's next byte and resumes
   without creating another backend file. Consecutive recovery attempts use
   bounded exponential backoff, malformed or backward ranges fail closed, and
   an expired session eventually returns a retryable failure so the outer
   idempotent upload path can re-check existence before opening a replacement
   session. Regression coverage proves protocol-aligned chunk boundaries,
   partial second-chunk commitment followed by probe-and-resume, bounded
   repeated failures, and continued idle-timeout enforcement. README,
   implementation guidance, and the repository learning now document the
   recovery contract. Verification passed with `yarn lint:fix`,
   `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, the 64-test Google Drive module suite,
   `cargo test --all-targets -- --test-threads=1` (567 passed, 3 ignored across
   targets), `cargo test --doc` (38 passed), and `git diff --check`. The
   existing manual checklist still documents real-Drive validation; the gated
   provider test remained ignored because external credentials were not
   supplied. The focused reviewer identified a genuine medium-severity
   reliability and duplicate-write risk, precisely distinguished creating a
   resumable session from implementing resumable transfer behavior, and
   requested the decisive chunk, committed-offset, backoff, and interruption
   tests. With one valid finding assessed here and no invalid finding
   attributable separately, this was high-quality, relevant feedback. See
   Google's [resumable upload
   guidance](https://developers.google.com/drive/api/guides/manage-uploads#resume-upload).

7. **[DONE] Drive error classification omitted important quota and permission
   reasons** (Medium, `src/google_drive.rs`, `src/error.rs`, `src/server.rs`, and
   `AGENTS.md`): **Valid and actionable.** The common classifier previously
   recognized only two Drive quota reasons, two rate-limit reasons, and
   authentication/scope failures. Documented HTTP 403 responses for account,
   folder, shared-drive, and daily capacity limits therefore became generic
   upstream failures instead of quota errors that direct operator remediation.
   Documented file ACL, domain policy, and shared-drive membership denials also
   had no accurate typed category, while `sharingRateLimitExceeded` was not
   retryable. The classifier now maps active-item, daily, folder-child,
   hierarchy-depth, storage, and shared-drive file limits to
   `StorageError::QuotaExceeded`; maps app authorization, domain policy, file
   permission, and shared-drive membership reasons to a new sanitized
   `StorageError::PermissionDenied`; and maps the remaining sharing rate limit
   to `StorageError::Retryable`. The Git LFS boundary reports permission denial
   as a non-retryable backend gateway failure without exposing provider
   diagnostics to clients. Table-driven regressions cover every newly
   classified capacity and permission reason, a focused regression covers the
   additional rate-limit reason, and server coverage proves the new denial
   category cannot be mistaken for a retryable response. The repository
   learning records why Drive's shared HTTP 403 status cannot be classified
   without its structured reason. Verification passed with `yarn lint:fix`,
   `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test --all-targets -- --test-threads=1` (580 passed,
   3 ignored across targets), and `cargo test --doc` (38 passed). The focused
   reviewer identified a genuine medium-severity operational correctness flaw,
   correctly distinguished retry, denial, and capacity remediation, and asked
   for the decisive representative tests. With one valid finding assessed here
   and no invalid finding attributable separately, this was high-quality,
   relevant feedback. See Google's [Drive error reason
   guidance](https://developers.google.com/workspace/drive/api/guides/handle-errors).

8. **[DONE] Flat root-folder storage creates a scaling ceiling and expensive
   list queries** (Medium, `src/google_drive.rs`, `src/server.rs`,
   `src/metadata.rs`, and Drive storage documentation): **Valid and
   actionable.** Uploads previously placed every object directly under the
   configured root, despite Drive's per-folder item limit, and production
   existence/download checks ignored SQLite's stored backend ID in favor of a
   paginated `files.list` query. New uploads now use one of 256 deterministic
   `lfs-cloud-sha256-<first-2>` folders. Private folder properties distinguish
   application shards from same-named operator folders; discovery validates
   every matching physical shard, tolerates concurrent duplicate shard-folder
   creation, and retains lookup for objects written directly under the root by
   older releases. Production first verifies SQLite's file ID with direct
   `files.get`; legacy root children remain root-scoped in one request, while a
   sharded child requires a second direct metadata request proving its folder
   belongs to the configured root and matches the expected SHA-256 prefix.
   Missing or mismatched IDs fall back to root-and-shard discovery. A found
   replacement repairs the backend ID without rewriting original creator
   attribution; absence conditionally marks only the unchanged mapping stale,
   so a concurrent upload cannot be overwritten. Regression coverage proves
   direct-ID lookup without list queries, shard-parent root validation,
   deterministic folder lookup/creation and upload placement, legacy root
   discovery, stale-ID repair, and race-safe stale marking. README,
   implementation guidance, manual verification instructions, and the
   repository learning now document the layout and repair contract.
   Verification passed with `yarn lint:fix`, `cargo fmt --all --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1` (576 passed, 3 ignored across
   targets), `cargo test --doc` (38 passed), and `git diff --check`. The focused
   reviewer identified a genuine medium-severity scalability and request-cost
   flaw and prescribed all three necessary boundaries: indexed direct reads,
   deterministic sharding, and stale-index repair. With one valid finding and
   no invalid finding attributable separately, this was high-quality, relevant
   feedback. See Google's [`files.get`
   reference](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/get)
   and [folder-limit
   guidance](https://developers.google.com/workspace/drive/api/guides/folder#file_and_folder_limits).

## CLI, configuration, and Git integration

1. **[DONE] URL redaction leaked credentials in combined malformed or scp-style
   inputs** (High, `src/git.rs`, `src/cli.rs`, and `AGENTS.md`): **Valid and
   actionable.** The display redactor previously treated URL parsing, scp-like
   userinfo redaction, and raw query/fragment redaction as mutually exclusive
   strategies. A parseable scp-like value containing a query or fragment could
   therefore return after redacting only those suffixes while exposing its
   credential prefix; a malformed hierarchical URL could take the opposite
   fallback and redact its userinfo while preserving secret-bearing suffixes.
   Redaction now removes raw query and fragment data first, then continues
   through either parsed URL userinfo redaction or the scp-like fallback, so no
   successful strategy can bypass another sensitive component. Regression
   coverage combines password userinfo, query secrets, and fragment secrets in
   both parseable scp-like and malformed hierarchical inputs, and asserts that
   none of the sentinel secrets survive display sanitization. The repository
   learning records the ordering requirement for future display boundaries.
   Verification passed with `cargo fmt --check`, focused redaction tests,
   `yarn lint:fix`, `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets -- --test-threads=1`, `cargo test --doc`, and
   `git diff --check`. A default-parallel all-target run exposed two existing
   load-sensitive credential-helper timeouts; both passed focused reruns before
   the complete serial suite passed. The focused reviewer found a genuine
   high-severity credential-disclosure flaw,
   correctly identified the non-composable redaction strategies, and requested
   the decisive combined-input regressions; with one valid finding assessed
   here and no invalid finding attributable separately, this was high-quality,
   security-relevant feedback.

2. **[DONE] GitHub owner and repository matching was case-sensitive** (Medium,
   `src/server_config.rs`, `src/server.rs`, `src/cli.rs`, `src/git.rs`, and
   `AGENTS.md`): **Valid and actionable.** GitHub's official REST documentation
   states that both the repository `owner` and `repo` path parameters are not
   case-sensitive, while status and migration planning previously compared
   those parsed remote components byte-for-byte and the server route resolver
   required the request path to use the configured spelling. A mixed-case clone
   of the same GitHub repository could therefore report no configured mapping
   and receive a route-not-configured response. Server configuration now owns a
   provider-aware repository identity matcher: host comparison ignores ASCII
   case, and GitHub owner/repository comparison does too, while the original
   configured and remote spelling remains available for diagnostics and
   provider calls. Status and migration use that shared matcher. GitHub route
   resolution ignores case only across the host/owner/repository identity
   prefix; the `.git/info/lfs` and operation suffixes remain case-sensitive.
   Configuration validation also canonicalizes GitHub route comparison keys so
   two mappings that differ only by identity casing are rejected instead of
   becoming ambiguous. Regression coverage proves mixed-case status, migration,
   and batch-route matching, preserves display spelling, rejects case-only
   duplicate routes, and confirms that protocol suffix casing is not relaxed.
   The repository learning records the provider-specific comparison boundary.
   Verification passed with `yarn lint:fix`, `cargo fmt`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
   focused reviewer found a genuine medium-severity interoperability flaw,
   correctly tied GitHub's identity semantics to configuration, CLI, and route
   behavior, and requested the decisive mixed-case regressions; with one valid
   finding assessed here and no invalid finding attributable separately, this
   was high-quality, relevant feedback.

3. **[DONE] `pull` could hang and buffer unbounded command output** (Medium,
   `src/cli.rs`, `README.md`, `IMPLEMENTATION.md`, and `AGENTS.md`): **Valid and
   actionable.** The production fetch path used `Command::output`, so neither
   stdout nor stderr had a memory bound and a stalled `git lfs fetch` had no
   deadline. Pull now runs the fetch in an owned process tree, drains stdout and
   stderr concurrently, retains at most 256 KiB per stream, and terminates the
   tree immediately when either cap is crossed. A six-hour execution deadline
   bounds otherwise stalled fetches while leaving room for large LFS transfers.
   If the direct child exits while a descendant still owns an output pipe, the
   runner gives normal pipe closure a short grace period and then stops the
   remaining process tree rather than blocking on reader completion. Fetch
   still completes before cache registration or worktree mutation. Regression
   coverage forces both streams to produce output without bound and verifies
   early overflow termination, then creates a delayed descendant and proves it
   cannot survive the timeout to write a marker. README and implementation
   guidance document the deadline and output bounds, and the repository
   learning records the process-tree ownership requirement. Verification
   passed with `yarn lint:fix`, `cargo fmt`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`,
   `scripts/manual/verify-pull-command.sh`, and `git diff --check`. The focused
   reviewer identified a genuine medium-severity reliability and memory-safety
   flaw, correctly connected pipe draining, retention limits, deadlines, and
   descendant cleanup, and requested the decisive stress and process-tree
   behavior; with one valid finding assessed here and no invalid finding
   attributable separately, this was high-quality, relevant feedback.

4. **[DONE] The login prompt echoed tokens and read input without a bound**
   (Medium, `src/cli.rs`, `Cargo.toml`, `README.md`, `IMPLEMENTATION.md`, and
   `AGENTS.md`): **Valid and actionable.** Production used unbounded
   `BufRead::read_line` for the local bearer token and made no distinction
   between an interactive terminal and piped automation input. Login now
   detects an interactive stdin, reads through a cross-platform terminal
   handle with echo disabled, restores the prior echo state before continuing,
   and retains the terminal handle's drop-time restoration as a failure-path
   fallback. Interactive and piped paths share a reader that retains at most
   1,027 bytes, rejects raw token input above the 1,024-byte session-token
   limit, accepts LF and CRLF delimiters, trims surrounding ASCII whitespace,
   and rejects invalid UTF-8 without reflecting input. Deterministic tests prove
   that terminal reads occur only while echo is disabled and that echo is
   restored, while piped-input tests cover the exact limit, bounded rejection,
   CRLF, and safe trimming. User and implementation documentation now describe
   the interactive and automation contracts, and the repository learning
   records why both paths must share the same bounded reader. Verification
   passed with `yarn lint:fix`, `cargo fmt`, focused login tests,
   `scripts/manual/verify-login-command.sh`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`.
   The focused reviewer identified a genuine medium-severity local secret
   exposure and memory-boundary flaw, specified the relevant terminal and
   piped cases, and requested the decisive regressions; with one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, security-relevant feedback.

5. **[DONE] URL safety rules differed between `init` and server configuration**
   (Medium, `src/http_transport.rs`, `src/init.rs`, and
   `src/server_config.rs`): **Valid and actionable.** CLI server-base validation
   rejected raw whitespace, control characters, backslashes, and path dot
   segments before URL parsing, while server configuration accepted forms that
   the URL parser could discard or reinterpret. HTTP route bases now use one
   shared typed validator for scheme/host, raw-input safety, trailing slashes,
   dot segments, credentials, queries/fragments, and protected transport. The
   only context-specific exception is the existing explicit non-loopback HTTP
   opt-in; CLI and config callers retain tailored guidance for enabling it.
   One shared accepted/rejected matrix covers the complete policy, while the
   config regression proves the previously accepted parser-normalized forms are
   denied. Operator documentation and the repository learning now state that
   CLI server bases, server public URLs, and GitHub API URLs share this policy.
   Verification passed with `yarn lint:fix`, `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo build`,
   `cargo test --all-targets`, `cargo test --doc`, and `git diff --check`. The
   focused reviewer identified a genuine medium-severity consistency and URL
   interpretation flaw, named both affected boundaries precisely, and proposed
   the appropriate shared-policy and matrix-test design; with one valid finding
   assessed here and no invalid finding attributable separately, this was
   high-quality, relevant feedback.

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
