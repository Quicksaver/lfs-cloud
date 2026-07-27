# Pre-Release Refactor and Cleanup Findings

Review of the LFS Cloud production code for refactor, cleanup, and simplification opportunities ahead of the initial release.

## Scope and method

- Reviewed all 19 modules under `src/` (~25k lines of production code; the remaining ~29k lines of the 54k total are in-file `#[cfg(test)]` modules).
- Cross-checked findings against `AGENTS.md` § Learnings so that deliberate, hard-won designs are not mistaken for accidents.
- Verified every claimed duplication, dead item, and line reference by direct search rather than inference.

Nothing in this document proposes a behavior, feature, performance, or output change. Everything is duplication removal, scaffolding reduction, or additive test coverage, except where a section is explicitly labelled as design work.

## Constraints that shaped these findings

Two project constraints were applied and materially changed several conclusions:

1. **Provider code must stay behind plugin-style abstractions.** Future Git and storage providers should be addable without modifying core code. This promoted two architectural findings (§1, §2) above everything else, and it bounds how cross-module deduplication may be done (§7.4).
2. **Useful automated test coverage must be preserved or improved.** Tautological or redundant tests may be changed or removed, but real coverage may not decrease. This reversed part of one suggestion (§6.2), constrained another (§6.3), reshaped a third (§6.1), and surfaced a class of findings where existing tests create a false sense of coverage (§4).

## Priority summary

| # | Finding | Kind | Est. impact | Risk | When |
| --- | --- | --- | --- | --- | --- |
| 3 | **[DONE / REVIEWED]** `StorageProvider` contract test | Additive tests | New coverage | None | Completed |
| 4 | **[DONE / REVIEWED]** Tests that exercise scaffolding, not production | Coverage repair | Removes false trust | None | Completed |
| 5 | **[DONE / REVIEWED]** Shared CLI `--server` validation | Consistency fix | Shared contract | Low | Completed |
| 6.4 | **[DONE / REVIEWED]** Four copies of child-process/pipe handling | Deduplication | Shared contract | Medium | Completed |
| 6.5 | **[DONE / REVIEWED]** Three copies of the `check-attr` pipeline | Deduplication | Shared contract | Medium | Completed |
| 7 | **[DONE / REVIEWED]** Mechanical duplication across modules | Deduplication | Shared contracts | Low | Completed |
| 6.1 | **[DONE / REVIEWED]** `dispatch` test boilerplate | Test restructure | Same coverage | Low | Completed |
| 8 | **[DONE / REVIEWED]** Dead public API | Public-surface cleanup | 24 lines removed | Low | Completed |
| 1 | No provider factory; 8 concrete-variant matches | Design | Extensibility | Medium | Scoped separately |
| 2 | Server transfer path bypasses the plugin trait | Design | Extensibility | High | Scoped separately |
| 9 | Deferred items | Design | Readability | High | After launch |

---

## 1. No provider factory: eight sites construct providers by matching concrete variants

Adding a provider today means editing eight call sites across three modules. Several are irrefutable `let` patterns that will only start failing to compile once a second variant exists.

| Location               | Site                                                    |
| ---------------------- | ------------------------------------------------------- |
| `src/server.rs:330`    | `production_session_store`                              |
| `src/server.rs:661`    | `github_auth_router_with_client`                        |
| `src/server.rs:760`    | `ProviderBatchAuthorizer::from_config`                  |
| `src/server.rs:1095`   | `GoogleDriveTransferStore::validate_storage_providers`  |
| `src/server.rs:1113`   | `GoogleDriveTransferStore::object_store_for_repository` |
| `src/cli.rs:1567`      | `migration_google_drive_storage`                        |
| `src/cli.rs:3775`      | `validate_status_storage`                               |
| `src/metadata.rs:1190` | `upsert_storage_provider`                               |

`src/server_config.rs` is the natural home for `StorageProviderConfig::build_provider()` and `RepositoryProviderConfig::build_provider()` returning trait objects, plus `provider_type()` / `backend_root_id()` accessors for the metadata upsert. That collapses eight matches to two.

Related placement problem: `src/cli.rs:1320-1434` defines `MigrationGoogleDriveStorage`, a ~115-line Drive-specific `StorageProvider` implementation living in the CLI layer, and `migration_google_drive_storage` (`src/cli.rs:1539`) hardcodes Drive construction. Both belong behind the same factory.

Two couplings worth recording even if they are not addressed now:

- `production_session_store` (`src/server.rs:322`) derives the durable-session encryption key from the GitHub PAT via `session_encryption_secret`. A second repository provider has no PAT, so this core concern needs a provider-neutral key source before a second provider is possible. Note that changing the key source invalidates existing durable sessions, which is exactly what `PAT_AUTHENTICATION_SESSION_MIGRATION` in `src/metadata.rs:227` had to handle previously.
- `production_session_store` and `github_auth_router_with_client` contain the identical "collect GitHub providers, match on slice, reject if more than one" block. It is duplicated today and is also the code that must change per-provider.

**Gain:** One place to register a provider. Eliminates the "did I update all eight?" failure mode, and removes a duplicated block. **Drawback:** Trait-object construction must return `ServerResult`, so the factory signature is slightly heavier than the current inline matches. The metadata upsert needs accessor methods rather than direct field reads.

## 2. The server transfer path bypasses the generic storage trait

`providers.rs:190` defines a clean `StorageProvider` trait, and `StorageProviderTransferStore` (`src/server.rs:831`) adapts it to the server's `LfsObjectTransferStore`. Production serving does not use that adapter. It uses `GoogleDriveTransferStore` (`src/server.rs:1059`), a provider-named implementation. The generic adapter is reachable only through `lfs_server_router_with_provider_adapters`, which is `#[doc(hidden)]`, documented as "a narrow test seam," and used only by `tests/local_end_to_end.rs`.

This is deliberate and recorded in `AGENTS.md`: _"`GoogleDriveObjectStore` implements the generic storage-provider trait for migration/direct storage flows; server LFS transfers still wrap it separately to record verified-object metadata."_

The consequence for plugin extensibility is that adding S3 requires a second `LfsObjectTransferStore`, not just a second `StorageProvider`. However, the divergence is one method deep, not architectural: the metadata recording and stale-ID repair in `lookup_and_repair_object` (`src/server.rs:1143`) is provider-generic. Only `lookup_object_by_backend_id` and the object-store construction are Drive-specific.

Likely shape: a generic `MetadataRecordingTransferStore<P: StorageProvider>` owning the metadata and repair logic, plus a small optional trait (for example `BackendIdLookup`) that Drive implements for the indexed fast path and other providers leave defaulted.

**Gain:** The plugin boundary becomes the path production actually runs through, so `tests/local_end_to_end.rs` stops covering a path production does not take. **Drawback:** Genuine design work on the upload and download hot path. The Drive sharding, lookup, and repair behavior is covered by four separate `Learnings` entries and must survive the move intact. This is the largest item in this document.

## 3. **[DONE / REVIEWED] `StorageProvider` lacked a shared contract test**

**Validity:** Valid and actionable. The trait's lifecycle and integrity semantics were previously distributed across prose, fakes, and Drive-specific tests, so a future provider could satisfy its own tests while violating namespace isolation, idempotency, or error-category expectations.

**Outcome:** `assert_storage_provider_contract` now exercises byte-identical upload/download round trips, foreign-namespace isolation, idempotent re-upload without duplicate backend objects, OID and size rejection before backend writes, precise `ObjectNotFound` failures, and safe documented `delete_or_mark_object` outcomes. The same contract runs against `FakeStorageProvider`, `GoogleDriveObjectStore`, and `MigrationGoogleDriveStorage`. The shared fixture lives under `tests/support`, while the Drive adapters use a loopback HTTP fixture that covers real resumable-upload behavior. This realizes the proposed additive plugin-contract coverage; the anticipated fixture and Drive-stub complexity is contained but real, including the fixture's dual use by integration tests and CLI unit tests.

**Assessment and commits:** The initial implementation was committed as `0d753747d65cf3f5f72bf6c7ec6c9406317953a8` against `fceb2cf9faa57e6e9e305c3d84adac755819355f`. Initial CodeRabbit review reported no findings, and initial Codex review found no actionable regression. Blast review then identified valid contract-fidelity gaps: lifecycle postconditions needed stronger assertions, the Drive fixture needed to exercise resumable chunks, production HTTP clients needed coverage, and staged-file verification should not consume the upload lock. Those findings were addressed in `0cb1a6ab9d4e7152a3eba7c7e9efd7956bfde60b`. The final CodeRabbit follow-up against `0d753747d65cf3f5f72bf6c7ec6c9406317953a8` was rate-limited and therefore recorded as the workflow-defined no-op. Final Codex review found one valid P2: migration created a token-backed Drive store before waiting on the durable upload lock, allowing the token to age while blocked. Token acquisition now happens after lock acquisition, while staged-file verification remains before the wait; the fix and durable-lock regression are in `b1ba01fdb9c4a468a4cc88f1632135e9fda9a9e1`. No assessment comments remain unaddressed.

**Verification:** The initial implementation passed Yarn linting, `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, all 576 library/integration-target tests, and all 29 doc tests. After the Blast follow-up and final Codex remediation, the final tree again passed formatting, markdown lint, Clippy with warnings denied, build, all-target tests, and doc tests.

## 4. **[DONE / REVIEWED] Tests that exercise scaffolding instead of production code**

These create a false impression of coverage. Repairing them raises real coverage.

### 4.1 **[DONE / REVIEWED] Free-space guardrail was tested against a copy**

`ensure_temp_space_for_upload_with_available_space` (`src/server.rs:2678`, `#[cfg(test)]`) is a test-only reimplementation of the first half of `reserve_with_available_space` (`src/server.rs:2551`). The test asserting upload free-space guardrails asserts against that copy, so the production reservation path is not what runs — including the shared capacity snapshot and `reserved_bytes` accounting that `[server] Upload staging admission` identifies as race-critical.

**Validity:** Valid and actionable. The test-only copy still existed and was called only by the guardrail test, while the production coordinator already exposed the deterministic free-space seam the test needed.

**Outcome:** Deleted the copy and changed `temp_space_guardrail_requires_expected_size_plus_headroom` to call `UploadStagingCoordinator::reserve_with_available_space` directly. The test now exercises the production checked arithmetic, shared capacity snapshot, reservation accounting, and release path.

### 4.2 **[DONE / REVIEWED] Dead production logic was kept alive by a test of itself**

`parse_ls_tree_blob_output` (`src/migration.rs:3494`, `#[cfg(test)]`, ~62 lines) parses `git ls-tree` output that `HistoryScanner` no longer produces; it was replaced by raw tree parsing (`parse_raw_git_tree`). Its only caller is its own test at `src/migration.rs:5607`.

**Validity:** Valid and actionable. Current-tree search confirmed that the parser was test-only, its sole caller was `ls_tree_parser_skips_non_blob_entries`, and production history scanning uses raw Git tree parsing.

**Outcome:** Deleted `parse_ls_tree_blob_output`, its test import, and its self-test. Existing history-scanner tests continue to exercise the raw production tree parser, including skipping LFS-matching gitlinks.

### 4.3 **[DONE / REVIEWED] Upload staging helpers bypassed real admission**

`stage_upload_request_body`, `stage_upload_request_body_with_limit`, and `stage_upload_request_body_with_guardrails` (`src/server.rs:2310-2375`, all `#[cfg(test)]`) each construct a throwaway `UploadStagingCoordinator::new(1, 1)`. Tests using them never exercise real admission or the per-user slot map, so the global and per-user limits described in `[server] Upload staging admission` are untested at this layer.

**Validity:** Valid and actionable as a coverage gap. Coordinator unit tests covered semaphore behavior, but no handler-level test proved that configured limits, authenticated stable-user principals, and overload responses were wired together. The three narrow helpers remain useful for post-admission body-staging tests and are not treated as admission coverage.

**Outcome:** Added handler-level tests that retain a real staging lease through a blocked backend upload. One sends a competing request for the same authenticated stable user while global capacity remains, proving the per-user limit; the other uses a second stable user, proving the process-wide limit. Both require immediate HTTP 503 Git LFS JSON responses with `Retry-After: 1`. Assessment then bounded the competing requests with deadlines and abort cleanup so a limiter regression fails instead of hanging the suite.

**Assessment and commits:** The initial implementation was committed as `a4b54bcc975271e6b804fea3c439df3a17512bf1` against `6c59c67d87fba8d0021bb0c62b2f5f0421309388`. Initial CodeRabbit review was rate-limited and recorded as the workflow-defined no-op. Initial Codex review found one valid P2: both new concurrency tests could deadlock indefinitely if admission regressed. Deadlines and guaranteed blocked-task cleanup were committed in `e587ee213ebe200d68d59224400f5146c7260601`. Blast review was skipped as the workflow-defined no-op after an upstream Claude quota failure; because it produced no remediation commit, `assess-work` stopped without the final CodeRabbit and Codex rounds.

**Verification:** The initial implementation passed Yarn linting, `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, all 578 non-ignored library tests, every integration target, and all 29 doc tests. The Codex remediation reran the required verification successfully. The original reviewer was high quality: all three findings were valid, current, and correctly distinguished dead scaffolding from a real missing handler-level test. Codex's single P2 was also valid and directly improved regression behavior; CodeRabbit and Blast quality could not be assessed because their steps were rate-limited.

## 5. **[DONE / REVIEWED] `login` and `init` validate `--server` differently**

- **Shared `--server` validation** (Low, `src/cli.rs`): **Partly stale but valid and actionable.** Production `login` already passed the raw argument through `LfsInitRoute::resolve_with_insecure_http`, so the stated command-level dot-segment acceptance example no longer reproduced. However, `auth_url_for_server` still maintained a weaker hand-written validator and could accept those unsafe forms when called directly. It now delegates to `init::validate_server_url`, and its regression matrix covers whitespace, control characters, backslashes, and raw or encoded dot segments.
- **Repeated route-segment appends** (Low, `src/cli.rs`): **Valid and actionable.** Login, logout, and the authenticated migration probe repeated `Url::path_segments_mut` composition. They now share `append_url_path_segments`; login and logout also share `auth_url_for_server`. Root and nested-base tests cover both auth routes.
- **Repeated redirect-disabled clients and runtime bridges** (Low, `src/cli.rs`): **Partly valid and actionable.** All three request paths repeated the redirect-disabled client builder, but only synchronous login and logout used `block_in_place` plus `block_on`; the migration probe was already async. `redirect_free_http_client` now centralizes the token-safety policy, while `block_on_reqwest` deduplicates only the genuine synchronous bridges and leaves the async probe async.
- **Repeated dehydration containment** (Low, `src/cli.rs`): **Valid and actionable.** `run_dehydrate_from_dir` already supplied a canonical contained regular-file path, but the indexed-object lookup repeated the parent canonicalization, containment, and symlink-metadata checks. The contained path now flows through the lookup directly; naming, comments, and absolute plus `..` traversal regressions make the caller precondition explicit. Claims that this introduced a traversal or symlink vulnerability were invalid because the caller still performs the full containment gate before reaching the optimized helper.

**User-facing documentation and release notes:** `docs/configuration.md` already documents the shared CLI route-base policy, including rejection of whitespace, control characters, backslashes, and dot segments, so the current guide became accurate without a separate wording change. This pre-release repository has no changelog file; `scripts/release.sh` generates GitHub release notes, so the user-visible tightening is carried by the descriptive `ccfa98088d9f48d7696415055a89dbb2c9e55c0e` implementation commit and its follow-up.

**Assessment and commits:** The implementation was committed as `ccfa98088d9f48d7696415055a89dbb2c9e55c0e` against `46ff9e5536ac3148d5e75de65ef1582fc734e513`. Initial CodeRabbit raised one invalid critical claim that the two-argument validator signature was obsolete; the current definition accepts exactly those arguments. Initial Codex found no actionable regression. Blast rejected three dehydration security claims that overlooked the caller containment gate, accepted useful contract clarity, composition, traversal-coverage, and helper-documentation feedback, and committed the resulting refinements as `ecacbfea91b7add0f795b6a75c96c3fcc5fa4319`. Claude Sonnet and Gemini Blast passes were quota-limited no-ops. Final CodeRabbit repeated the same invalid validator-signature claim; final Codex found no actionable defects. No assessment comments remain unaddressed.

**Verification:** The initial implementation passed `cargo fmt --all`, Yarn linting, `git diff --check`, Clippy across all targets with warnings denied, `cargo build`, all 578 non-ignored library tests, every integration target, and all 29 doc tests. The Blast remediation reran formatting, linting, Clippy, build, all-target tests, and doc tests successfully; final CodeRabbit also passed `git diff --check` and `cargo check`.

**Reviewer quality:** The original review was useful and found four worthwhile consistency or duplication issues, but its central command-level reproduction and three-way blocking-runtime claim were not current-tree accurate. CodeRabbit was low quality here because both passes repeated one factually incorrect critical finding. Codex was precise in both clean passes. Blast was mixed but productive: it rejected existing caller guarantees in three security claims, while its valid maintainability and regression-test suggestions materially clarified the final implementation.

## 6. **[DONE / REVIEWED] Scaffolding and duplication with test implications**

### 6.1 **[DONE / REVIEWED] `dispatch` test boilerplate**

`dispatch` (`src/cli.rs:287-334`) takes eleven closures so that ten tests (`src/cli.rs:4473-4850`, ~340 lines) can each assert "subcommand X calls runner X" with nine `unreachable!()` arms apiece.

These tests do cover something a parser-only test would miss: that each match arm maps the correct variant to the correct runner. Swapping `Command::Pull => hydrate(...)` would slip past a parse assertion. The coverage is therefore worth keeping.

**Action:** Keep `dispatch` and keep the coverage; replace ten tests with one table-driven test. Define an `enum Invoked { Serve, Init, Login, ... }`, pass a single shared recorder as all eleven runners, loop over the ten subcommand argv vectors, and assert the recorded variant plus the parsed fields.

**Gain:** Identical assertion surface in roughly 40 lines instead of 340, and it scales down rather than up as subcommands are added. Also allows asserting all parsed fields, not just the one value each current test captures. **Drawback:** None to coverage. Whether to additionally collapse the eleven generic parameters into a single `trait CommandRunner` object is separable and optional; the recorder harness works either way.

**Validity and outcome:** Valid. The ten repetitive tests became one table-driven `dispatches_every_subcommand_to_its_matching_runner` test. It retains every runner edge and now compares each complete parsed command/config value, so dispatch coverage did not become parser-only coverage. The production closure seam remains unchanged.

### 6.2 **[DONE / REVIEWED] Credential constructor ladder (partly retracted)**

`GitCredentialApproval`, `GitCredentialRejection`, and `GitCredentialLookup` each expose a four-deep ladder: `new` -> `with_username` -> `*_in_dir` -> `*_with_git_program` -> `*_with_git_program_in_dir`.

The `*_with_git_program*` forms have no production callers but are load-bearing test seams: tests inside `src/credentials.rs` use them to inject a fake `git` executable, covering behavior recorded in `[auth] Credential helper process groups` and `[tests] Process-tree portability`. **Keep them.**

What remains is thin: the no-argument `approve()`, `reject()`, and `lookup()` forms that only default the directory to `"."`, plus the three `with_username` constructors. Roughly 40 lines; low value.

Unaffected by this retraction, and still worth doing at full strength: `src/credentials.rs` contains four helper pairs where the stderr-only variant is the general one with `retain_stderr_data` and no stdout reader — `wait_for_git_command_timeout` / `wait_for_git_command_output`, `drain_available_stderr` / `drain_available_pipe`, `drain_stderr_until` / `drain_pipe_until`, and `finish_stderr_reader_after_child_exit` / `finish_output_readers_after_child_exit`.

**Gain:** ~120 lines. Make stdout an `Option<PipeReader>` in the general path and delete the four narrow variants. **Drawback:** None material. The code is already parameterized by `retain: fn(&mut Vec<u8>, &[u8])`; the abstraction exists and is only half-applied.

**Validity and outcome:** Partly valid. The pipe-helper duplication was real and is now routed through the shared child-process runner. The proposed constructor and current-directory convenience removal was rejected: `new`, `with_username`, `approve`, `reject`, and `lookup` are public APIs with current production, integration, test, or documentation use. All `*_with_git_program*` fake-Git seams remain intact.

### 6.3 **[DONE / REVIEWED] Local cache duplication (seams excluded)**

The closure ladders `hydrate_pointer_file` -> `hydrate_pointer_file_with_before_publish` and `dehydrate_file` -> `dehydrate_file_with_before_pointer_publish` -> `dehydrate_file_with_read_observer` exist to inject callbacks into the GC and publication race window. Those tests cover the invariants in `[cache] GC operation boundary` and `[cache] Worktree replacement races`. **Keep the seams.**

Only the mechanically identical bodies should be merged:

- `copy_cache_object_to_temporary_file` (`:2146`), `copy_and_verify_object` (`:2811`), and `hash_open_file` (`:2879`) are three copies of the same read-64KiB, hash, count, overflow-check loop.
- `ensure_cache_object_file` (`:2578`), `ensure_source_object_file` (`:2601`), and `cache_object_path_exists` (`:2561`) are three identical `fs::metadata` matches differing only in the `NotFound` arm.
- `read_lfs_pointer_file` (`:1853`), `read_existing_lfs_pointer_file` (`:1899`), and `collect_pointer_oid_from_file` (`:1626`) are three bounded pointer readers returning error, `Option`, and a side effect respectively.
- `verify_file_object` and `verify_worktree_file_object` repeat the same mismatch block.
- `entry_is_directory` and `entry_is_file` are trivially one function.

**Gain:** ~200 lines and a substantially shorter file. **Drawback:** The mode-carrying distinctions are load-bearing — `CachePublishDurability::Recoverable` vs `Durable`, `MaterializationMode::NoReplace` vs `ReplaceMatchingPointer`. Leave those functions alone.

**Validity and outcome:** Valid within the stated seam boundary. Shared helpers now cover entry-kind and regular-file checks, bounded pointer reads, object-identity checks, and the 64 KiB hash/count/overflow/copy loop. `hydrate_pointer_file_with_before_publish`, `dehydrate_file_with_before_pointer_publish`, `dehydrate_file_with_read_observer`, `CachePublishDurability`, and `MaterializationMode` remain separate. Assessment additionally restored nonblocking, regular-file-only GC pointer probes and added FIFO, symlink, and file-type-race regressions.

### 6.4 **[DONE / REVIEWED] Four independent implementations of child-process and pipe handling**

| Location | Implementation |
| --- | --- |
| `src/cli.rs:2574-2852` | `run_bounded_child_command`, `read_pipe_with_hard_limit`, `configure`/`terminate`/`stop_child_process_tree`, `signal_child_process_group` (~280 lines) |
| `src/migration.rs:3882-4105` | `run_bounded_command_output`, `read_pipe_with_hard_limit`, `configure`/`terminate`/`stop_bounded_git_process_tree`, `signal_bounded_git_process_group` (~225 lines) |
| `src/credentials.rs:1074-1580` | `wait_for_git_command_timeout` / `wait_for_git_command_output`, `PipeReader`, its own `stop_child_process_tree` (~500 lines) |
| `src/local_cache.rs:1507-1555` | A fourth, simpler spawn-plus-thread-writer variant |

The `#[cfg(unix)] signal_process_group` / `#[cfg(windows)] taskkill /T /F` / `#[cfg(not(any(unix, windows)))]` triple is written out three times verbatim. `read_pipe_with_hard_limit` is byte-identical in `cli.rs` and `migration.rs`.

The coverage argument is the strongest reason to do this. `AGENTS.md` records that process-tree timeout cleanup is verified by `command_timeout_stops_descendant_helpers` in `src/credentials.rs` — one thorough regression test, against one of four implementations. The other three do not get that contract.

**Gain:** ~500–600 lines removed, and the existing thorough test starts covering all call sites. This is the subsystem with the most hard-won platform knowledge in it (six `Learnings` entries); today a fix to one copy silently leaves three others wrong. **Drawback:** The four callers have genuinely different needs — stdout plus stderr vs stderr only, timeout vs no timeout, `Output` vs a custom struct. The unified API needs two or three knobs or it will grow its own ladder. Touches every subprocess path, so it wants `cargo test --all-targets` plus the manual verifiers in `scripts/manual/` before it is trusted.

**Validity and outcome:** Valid. `src/child_process.rs` now owns process-group setup, recursive termination, bounded or truncated concurrent pipe capture, timeouts, inherited-pipe policy, and bounded cleanup; CLI, credentials, migration, and local-cache callers retain their domain-specific invocation and error mapping. Assessment made captured output secret-safe, fixed descendant cleanup and post-exit hard-limit propagation, and added escaped-descendant regressions proving timeout and output-limit cleanup cannot block on retained pipes.

### 6.5 **[DONE / REVIEWED] Three implementations of the `git ls-files -z` plus `git check-attr` pipeline**

`src/cli.rs:2887-2994`, `src/migration.rs:2380-2620`, and `src/local_cache.rs:1443-1624`. Each performs: NUL-delimited `ls-files`, a concurrent stdin write to `check-attr`, split on `\0`, `chunks_exact(3)`, validate `filter` / `lfs`, then a path-safety check with its own `#[cfg(unix)] OsStringExt` / `#[cfg(not(unix))] from_utf8` pair. Three copies of `git_path_bytes_to_path_buf`, three of the containment check.

**Gain:** ~200 lines, one thorough test suite instead of three partial ones, and one place to maintain the non-UTF-8 and Windows path handling that already has three `Learnings` entries. **Drawback:** The three invocations differ in real ways — `local_cache` uses `--cached` with `--git-dir` and `--work-tree`, `migration` uses `--source=<commit>` with input batching. The shared piece is the parse and path-safety half, not the invocation. Extracting only that is the safe scope.

**Validity and outcome:** Valid at the proposed parse/path boundary. `src/git_output.rs` now owns NUL-triple parsing, exact `filter=lfs` selection, raw-byte Unix paths, non-UTF-8 rejection elsewhere, and relative-contained path validation. Each caller still owns its different `ls-files`/`check-attr` invocation, batching, stdin writer, source/index mode, and domain error.

**Assessment and commits:** The implementation was committed as `e0f1c8078dc5c49fb44504f855573f1ac645aaaa` against `ade90afc0556fc65d415e2ccada9a52e02ab9522`. Initial CodeRabbit found one valid non-Unix lint issue, fixed in `bf141f77aac16b121e1a2ebb0af1bfa3aeff3e3d`. Initial Codex found one valid P2: GC pointer candidates could block on FIFOs; regular-file preflight, nonblocking Unix opens, and regressions were committed in `44b78a0cc581326d82c4cd5eb08865dfc1a2f977`. Blast accepted the valid process secrecy/cleanup and cache path-race findings, rejected speculative or factually stale claims, and committed fixes in `c2f6683f7c67f48ae21216c6ae09145d3cfe8c68`; its Sonnet and Gemini passes were quota-limited no-ops. Final CodeRabbit found two valid process cleanup/limit-propagation gaps, fixed in `065a266213005fa7e6296630438bf3663a5ee52c`. Final Codex found one valid P1 unbounded reader join, reproduced with escaped descendants and fixed in `faf34ddae900245b15f5132c97e7e6d905008c24`. A final Claude Opus 5 single-model review found twelve valid follow-ups: direct Unix process-group signaling, event-driven pipe polling, accurate inherited-pipe documentation, portable and readiness-synchronized subprocess tests, restored credential truncation/redaction coverage, explicit capture policies, precise cache subprocess diagnostics, reported non-regular GC pointer candidates, one bounded-read cleanup, a generic migration error, and the missing shared Git-output learning. All were addressed without weakening the no-follow GC boundary. No assessment comments remain unaddressed.

**Verification:** The final tree passed Yarn linting, `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, 587 library tests (with 8 more ignored), every integration target (32 passed and 3 live-provider tests ignored), and all 29 doc tests. Final-tree manual verification passed credential-helper fallback, APFS/local-cache materialization, hydrate/dehydrate plus Git LFS push, cache GC, login, logout, status, pull, historical migration execution, source fetch, and secret-redaction checks. Live GitHub and Google Drive tests remained gated because this mechanical local refactor did not provide external credentials. The Claude Opus 5 follow-up again passed Yarn linting, formatting, diff checks, host Clippy with warnings denied, `cargo build`, all 621 test-target tests (12 expected ignores), and all 29 doc tests. A Windows-target Clippy attempt stopped in `aws-lc-sys` before crate compilation because the local environment lacks `x86_64-w64-mingw32-gcc`; native Windows coverage remains with CI.

**Reviewer quality:** The original review was high quality overall: §6.1, §6.3, §6.4, and §6.5 were current and correctly preserved their coverage and race/mode boundaries. §6.2's pipe consolidation was valid, but its constructor/convenience-removal remainder understated current public and production use. CodeRabbit and Codex were consistently precise and found valid cross-platform, availability, and bounded-cleanup defects. Blast was productive but mixed: Claude Opus was strong, while Grok included several duplicated or speculative claims alongside useful race findings. The final Claude Opus 5 pass was exceptionally precise: all twelve comments were current, in scope, and actionable, ranging from cross-platform test correctness and security-sensitive process signaling to operator diagnostics and small maintainability fixes.

## 7. **[DONE / REVIEWED]** Mechanical duplication

### 7.1 **[DONE / REVIEWED]** Google Drive URL builders

**Validity and outcome:** Valid and actionable. `drive_api_url` now owns the configurable proxy-prefix and trailing `/drive/v3` rules, `drive_files_url` supplies the common files endpoint, and `require_drive_identifier` retains the endpoint-specific blank-ID diagnostics. Query fields remain beside each endpoint. The resumable-upload exception is explicit in `DriveApiEndpoint::ResumableUpload`: it removes an existing Drive suffix before appending `/upload/drive/v3/files`. The existing learning now points at this canonical enforcement point.

### 7.2 **[DONE / REVIEWED]** Google Drive URL validators

**Validity and outcome:** Valid and actionable. `validate_drive_url` shares parsing, absolute HTTP(S), exact literal-loopback HTTP, credential, and fragment checks; `DriveUrlComponentPolicy` preserves the real distinction that API bases reject queries while resumable session URLs allow them. `drive_upstream_error` centralizes secret-safe status-less provider errors. Assessment caught and fixed the initial use of the older named-loopback helper, then added API and session regressions proving that `localhost` is rejected while literal IPv4 and IPv6 loopback addresses remain accepted. `docs/configuration.md` and the Drive safety learning now state the literal-IP rule consistently.

### 7.3 **[DONE / REVIEWED]** `metadata.rs` boilerplate

**Validity and outcome:** Valid and actionable. The five async wrappers now use `run_blocking`, retaining owned inputs, Tokio blocking-pool dispatch, `MetadataTaskJoin`, and the database path. `METADATA_MIGRATIONS` preserves ascending versions 2 through 5, including PAT-session invalidation, while tests assert contiguity and alignment with `METADATA_SCHEMA_VERSION`. `migration_error` and `operation_error` centralize path-preserving mappings within `MetadataDatabase`; path-aware free helpers remain explicit. Existing migration and SQLite responsiveness tests continue to exercise ordering and the async boundary.

### 7.4 **[DONE / REVIEWED]** Small cross-file duplicates

**Validity and outcome:** Partly stale after §6, but the remaining duplication was valid and actionable. The neutral `process_output` module now owns canonical signal-status text, UTF-8-safe bounded lossy diagnostics, and UTF-8-boundary ellipsis truncation. Credentials retain their deliberately richer platform status rendering and domain-specific redaction order. Git command spawning, success handling, bounded decoding, and migration array/vector argument variants share generic cores while preserving optional-config exit semantics and command-specific error enums. Drive and GitHub bounded error-body reads share `read_bounded_lossy_response_body` in neutral `http_transport`; no provider module depends on another provider. The upload response reader remains separate because it resets an idle timeout between chunks.

### 7.5 **[DONE / REVIEWED]** Server error mappings

**Validity and outcome:** Valid and actionable. `classify_lfs_storage_error` is the single variant table and carries upload status/message, download status/message, and batch code/message independently. This preserves the intentional integrity-mismatch split (upload 422 versus download 502) and every existing client-facing phrase. `finish_failed_transfer_attempt` now calls `server_error_log_category`; assessment added regression coverage proving nested storage and repository-provider categories remain intact.

### 7.6 **[DONE / REVIEWED]** Batch response builders

**Validity and outcome:** Valid and worth consolidating in the current tree. `batch_objects_with_storage_lookup` shares deduplication, bounded ordered lookups, and request-order reconstruction through a function-pointer mapper, avoiding a trait over unrelated response enums. Download and upload keep small explicit outcome mappers, so their available/missing and present/needed semantics remain obvious. Existing tests prove duplicate requests still count in responses, perform one storage lookup per unique object, and preserve request order.

**Assessment and commits:** The verified implementation was committed as `0898b9c9e5fe5d00bf992f909cd7b05943146e29` against `a3693342e0047c5ce4ae1652920a683db5e8564b`. Initial CodeRabbit found one valid minor literal-loopback regression, fixed in `c9906e0c19dac6c9a523762695abd2cebc103981`; initial Codex found no actionable defect. Blast produced `ee3044ddff4fc7c699bd867d40fe540aef1e56cf`, addressing valid helper-contract, migration-invariant, UTF-8 truncation, Git cleanup, Drive guidance, and error-classification feedback, while one transfer-category comment was already satisfied and received regression coverage. Claude Sonnet and Gemini Blast passes were quota-limited no-ops. Final CodeRabbit and final Codex found no actionable defects.

**Verification:** Focused Drive, metadata, server, Git, migration, credentials, and CLI tests passed. The implementation and assessment commits passed Yarn linting, `cargo fmt --all`, `git diff --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, all 621 automated test-target tests (12 expected helper/live-provider tests ignored), and all 29 doc tests.

**Reviewer quality:** The original reviewer was high quality: all six groups identified real consolidation opportunities, correctly required a neutral provider boundary, and explicitly preserved query, upload-path, migration, error-message, and batch-readability distinctions. Its §7.4 status-text claim became partly stale after §6 because credentials now intentionally render signal termination differently, but the remaining items were current. Initial CodeRabbit was precise (one valid finding out of one). Both Codex passes were clean and relevant. Blast was productive: Claude Opus feedback was fully relevant, Grok supplied four valid comments out of five, and the unavailable Sonnet and Gemini passes cannot be assessed.

## 8. **[DONE / REVIEWED]** Dead public API

| Item | Review status | Validity and outcome |
| --- | --- | --- |
| `fetch_authenticated_github_user` (formerly `src/github_auth.rs:708`) | **[DONE / REVIEWED] Removed** | Valid and actionable. It had no callers, merely selected `GitHubUserClient::new`, and exposed GitHub-specific convenience rather than a provider extension seam. The function and root re-export were removed before the first release; `GitHubUserClient` remains public for explicit client use. |
| `GoogleDriveRootValidator::with_client` (formerly `src/google_drive.rs:489`) | **[DONE / REVIEWED] Removed** | Valid and actionable. It had no callers and only supplied `GOOGLE_DRIVE_API_BASE_URL` to `with_client_and_api_base_url`. The strict-subset constructor was removed; `new` retains the production default and the full constructor retains client and endpoint injection. Assessment clarified that the full constructor accepts the public production constant or a validated loopback test URL. |
| `lfs_server_router` (`src/server.rs:347`) | **[DONE / REVIEWED] Retained intentionally** | Valid recommendation to keep. Although it has no in-tree caller beyond its root re-export, it is the public zero-setup Axum router constructor for embedders. It is an extension-facing composition entry point, unlike the removed provider-specific conveniences, and remains part of the §9.2 router-builder design rather than this cleanup. |

**Public-surface reasoning:** The root re-export block remains the prospective plugin/public surface and was not trimmed wholesale. Removing two unused public conveniences is an intentional pre-release API break that narrows redundant choices before SemVer stability; the underlying public clients and configurable constructors remain available. The embedder-facing router entry point remains exported because absence of an in-tree caller does not make an extension seam dead.

**Assessment and commits:** The two removals were committed in `586805f64889bb69f23a37a0e586d49ce598911f` against `929779e1ce29a4983d72b072fbafecdd286b35c9`. Initial CodeRabbit and Codex reviews found no actionable defects. Blast found one valid documentation ambiguity in the surviving Drive constructor and committed the clarification in `f7c3525c51f16ab8374e99dce6223c8583f30c30`; its removed-reference check found no stale references in doctests, tests, README, or current guides. Claude Sonnet and Gemini Blast passes were quota-limited no-ops. Final CodeRabbit and final Codex found no actionable defects.

**Verification:** The implementation and assessment commits passed `cargo fmt --all`, `git diff --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, all 627 automated test-target tests (12 expected helper/live-provider tests ignored), and all 29 doc tests. Direct current-tree searches confirmed that only `lfs_server_router` remains in source and the root re-export; the removed names remain only here as historical reviewed items.

**Reviewer quality:** The original reviewer was high quality: all three API classifications were current and correctly distinguished redundant provider conveniences from an intentional embedder seam (three valid conclusions out of three). CodeRabbit and Codex were clean and relevant in both passes. Blast's available Claude Opus feedback was precise: it found one useful documentation improvement and correctly requested stale-reference verification; the quota-limited Sonnet and Gemini passes cannot be assessed.

## 9. Deferred until after launch

### 9.1 `handle_lfs_upload_request` is ~290 lines

`src/server.rs:1904-2190`. Every error path repeats `finish_failed_transfer_attempt(...)`, then `tracing::debug!(repo_id, oid, error_category, ...)`, then `return git_lfs_storage_error_response(error)` — six times.

An inner `async fn` returning `Result<Response, (ServerError, &'static str)>` plus one epilogue at the call site would roughly halve it and make the actual sequence (authorize, attempt, lock, lookup, stage, upload) legible in one screen.

**Drawback:** The most safety-critical handler in the codebase, with several ordering invariants documented in `Learnings`. Restructuring risks subtly changing which failure paths record which attempt status. Defer.

### 9.2 Six-level telescoping router constructors

`lfs_server_router` -> `lfs_server_router_with_sessions` -> `..._and_authorizer` -> `..._authorizer_and_transfer_store` -> `..._authorizer_transfer_store_and_batch_guardrails` -> `build_lfs_server_router` (`src/server.rs:347-609`). Names encode their parameter lists. `lfs_server_router_with_sessions_and_authorizer` has exactly one caller. `server_router_with_sessions` and `server_router_with_sessions_and_transfer_store` (`:360-417`) duplicate the same auth/session/LFS merge block verbatim.

A small `LfsRouterBuilder` collapses six functions to one and makes the roughly 40 test call sites read as intent rather than as an arity puzzle. A builder is also the right shape for plugin composition: adding an injection axis becomes a method rather than a seventh function name.

**Drawback:** Roughly 40 test call sites change shape. Mechanical, but a wide diff. Consider pulling this forward if §1 is done, since the provider factory and the router builder touch the same composition code.

---

## Suggested sequencing

**Before launch, additive only, no behavior risk**

1. §3 `assert_storage_provider_contract` harness.
2. §4 Repair the three scaffolding-tests-that-test-scaffolding.
3. §5 `auth_url_for_server` validation consistency.

**Before launch, mechanical and coverage-increasing**

4. §6.4 process helpers, §6.5 `check-attr` pipelines.
5. §7.3 metadata boilerplate, §6.2 credentials pipe variants, §7.4 small duplicates into a neutral module.
6. §6.1 table-driven dispatch test, §7.5 error-mapping tables, §8 the two verified-dead items.
7. §7.1, §7.2 Drive URL builders and validators, §6.3 local cache bodies.

**Deliberate design work, scoped separately from cleanup**

8. §1 provider factory.
9. §2 generic metadata-recording transfer store.
10. Provider-neutral session encryption key (blocks a second repository provider outright).

**After launch**

11. §9.1 upload handler restructure.
12. §9.2 router builder, unless pulled forward with §1.

## Caveat

The test suite is larger than the production code (~29k vs ~25k lines), and much of the scaffolding flagged here exists to serve it. Every consolidation will ripple into tests. Sequence by test blast radius rather than by lines saved.
