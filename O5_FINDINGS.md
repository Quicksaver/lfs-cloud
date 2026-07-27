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
| 5 | `login` and `init` validate `--server` unequally | Consistency fix | ~30 lines | Low | Before launch |
| 6.4 | Four copies of child-process/pipe handling | Deduplication | ~500–600 lines | Medium | Before launch |
| 6.5 | Three copies of the `check-attr` pipeline | Deduplication | ~200 lines | Medium | Before launch |
| 7 | Mechanical duplication across modules | Deduplication | ~700 lines | Low | Before launch |
| 6.1 | `dispatch` test boilerplate | Test restructure | ~300 lines | Low | Before launch |
| 8 | Dead public API | Removal | ~80 lines | Low | Before launch |
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

## 5. `login` and `init` validate `--server` differently

`auth_url_for_server` (`src/cli.rs:3792`) hand-rolls URL validation that `init::validate_server_url` -> `http_transport::validate_http_url` already performs, but more weakly: it omits the whitespace, control-character, and backslash check, and it omits the dot-segment check.

Concretely, `login --server` routes through `auth_url_for_server`, while `logout --server` and `init --server` route through `validate_server_url`. So `--server https://host/a/../b` is rejected by `init` and accepted by `login`.

`AGENTS.md` `[config] Shared HTTP route bases` explicitly requires these to share the validator.

**Action:** Route `auth_url_for_server` through `validate_server_url`; delete the hand-rolled checks.

Related, in the same area:

- Three helpers append path segments to a validated server URL: `session_revocation_url_for_server` (`src/cli.rs:808`), `auth_url_for_server` (`src/cli.rs:3792`), and an inline block in `probe_authenticated_migration_target` (`src/cli.rs:3698`).
- Three sites build `Client::builder().redirect(Policy::none())` and wrap it in the same `block_in_place` plus `block_on` dance.
- `run_dehydrate_from_dir` (`src/cli.rs:1162-1171`) calls `contained_worktree_file_path`, then `indexed_lfs_object_for_dehydration` -> `dehydration_relative_path` -> `contained_worktree_file_path` again. Net effect per path: `dunce::canonicalize(worktree_root)` runs three times, and the parent canonicalize plus `symlink_metadata` twice. Harmless but confusing; pass the already-contained path down.

**Gain:** One validation contract across all CLI entry points, ~60 lines removed. **Drawback:** Tightening `login` may reject a server URL a user could previously pass. That is the intended behavior and matches `init`, but it is a user-visible change and belongs in release notes.

## 6. Scaffolding and duplication with test implications

### 6.1 `dispatch` test boilerplate

`dispatch` (`src/cli.rs:287-334`) takes eleven closures so that ten tests (`src/cli.rs:4473-4850`, ~340 lines) can each assert "subcommand X calls runner X" with nine `unreachable!()` arms apiece.

These tests do cover something a parser-only test would miss: that each match arm maps the correct variant to the correct runner. Swapping `Command::Pull => hydrate(...)` would slip past a parse assertion. The coverage is therefore worth keeping.

**Action:** Keep `dispatch` and keep the coverage; replace ten tests with one table-driven test. Define an `enum Invoked { Serve, Init, Login, ... }`, pass a single shared recorder as all eleven runners, loop over the ten subcommand argv vectors, and assert the recorded variant plus the parsed fields.

**Gain:** Identical assertion surface in roughly 40 lines instead of 340, and it scales down rather than up as subcommands are added. Also allows asserting all parsed fields, not just the one value each current test captures. **Drawback:** None to coverage. Whether to additionally collapse the eleven generic parameters into a single `trait CommandRunner` object is separable and optional; the recorder harness works either way.

### 6.2 Credential constructor ladder (partly retracted)

`GitCredentialApproval`, `GitCredentialRejection`, and `GitCredentialLookup` each expose a four-deep ladder: `new` -> `with_username` -> `*_in_dir` -> `*_with_git_program` -> `*_with_git_program_in_dir`.

The `*_with_git_program*` forms have no production callers but are load-bearing test seams: tests inside `src/credentials.rs` use them to inject a fake `git` executable, covering behavior recorded in `[auth] Credential helper process groups` and `[tests] Process-tree portability`. **Keep them.**

What remains is thin: the no-argument `approve()`, `reject()`, and `lookup()` forms that only default the directory to `"."`, plus the three `with_username` constructors. Roughly 40 lines; low value.

Unaffected by this retraction, and still worth doing at full strength: `src/credentials.rs` contains four helper pairs where the stderr-only variant is the general one with `retain_stderr_data` and no stdout reader — `wait_for_git_command_timeout` / `wait_for_git_command_output`, `drain_available_stderr` / `drain_available_pipe`, `drain_stderr_until` / `drain_pipe_until`, and `finish_stderr_reader_after_child_exit` / `finish_output_readers_after_child_exit`.

**Gain:** ~120 lines. Make stdout an `Option<PipeReader>` in the general path and delete the four narrow variants. **Drawback:** None material. The code is already parameterized by `retain: fn(&mut Vec<u8>, &[u8])`; the abstraction exists and is only half-applied.

### 6.3 Local cache duplication (seams excluded)

The closure ladders `hydrate_pointer_file` -> `hydrate_pointer_file_with_before_publish` and `dehydrate_file` -> `dehydrate_file_with_before_pointer_publish` -> `dehydrate_file_with_read_observer` exist to inject callbacks into the GC and publication race window. Those tests cover the invariants in `[cache] GC operation boundary` and `[cache] Worktree replacement races`. **Keep the seams.**

Only the mechanically identical bodies should be merged:

- `copy_cache_object_to_temporary_file` (`:2146`), `copy_and_verify_object` (`:2811`), and `hash_open_file` (`:2879`) are three copies of the same read-64KiB, hash, count, overflow-check loop.
- `ensure_cache_object_file` (`:2578`), `ensure_source_object_file` (`:2601`), and `cache_object_path_exists` (`:2561`) are three identical `fs::metadata` matches differing only in the `NotFound` arm.
- `read_lfs_pointer_file` (`:1853`), `read_existing_lfs_pointer_file` (`:1899`), and `collect_pointer_oid_from_file` (`:1626`) are three bounded pointer readers returning error, `Option`, and a side effect respectively.
- `verify_file_object` and `verify_worktree_file_object` repeat the same mismatch block.
- `entry_is_directory` and `entry_is_file` are trivially one function.

**Gain:** ~200 lines and a substantially shorter file. **Drawback:** The mode-carrying distinctions are load-bearing — `CachePublishDurability::Recoverable` vs `Durable`, `MaterializationMode::NoReplace` vs `ReplaceMatchingPointer`. Leave those functions alone.

### 6.4 Four independent implementations of child-process and pipe handling

| Location | Implementation |
| --- | --- |
| `src/cli.rs:2574-2852` | `run_bounded_child_command`, `read_pipe_with_hard_limit`, `configure`/`terminate`/`stop_child_process_tree`, `signal_child_process_group` (~280 lines) |
| `src/migration.rs:3882-4105` | `run_bounded_command_output`, `read_pipe_with_hard_limit`, `configure`/`terminate`/`stop_bounded_git_process_tree`, `signal_bounded_git_process_group` (~225 lines) |
| `src/credentials.rs:1074-1580` | `wait_for_git_command_timeout` / `wait_for_git_command_output`, `PipeReader`, its own `stop_child_process_tree` (~500 lines) |
| `src/local_cache.rs:1507-1555` | A fourth, simpler spawn-plus-thread-writer variant |

The `#[cfg(unix)] signal_process_group` / `#[cfg(windows)] taskkill /T /F` / `#[cfg(not(any(unix, windows)))]` triple is written out three times verbatim. `read_pipe_with_hard_limit` is byte-identical in `cli.rs` and `migration.rs`.

The coverage argument is the strongest reason to do this. `AGENTS.md` records that process-tree timeout cleanup is verified by `command_timeout_stops_descendant_helpers` in `src/credentials.rs` — one thorough regression test, against one of four implementations. The other three do not get that contract.

**Gain:** ~500–600 lines removed, and the existing thorough test starts covering all call sites. This is the subsystem with the most hard-won platform knowledge in it (six `Learnings` entries); today a fix to one copy silently leaves three others wrong. **Drawback:** The four callers have genuinely different needs — stdout plus stderr vs stderr only, timeout vs no timeout, `Output` vs a custom struct. The unified API needs two or three knobs or it will grow its own ladder. Touches every subprocess path, so it wants `cargo test --all-targets` plus the manual verifiers in `scripts/manual/` before it is trusted.

### 6.5 Three implementations of the `git ls-files -z` plus `git check-attr` pipeline

`src/cli.rs:2887-2994`, `src/migration.rs:2380-2620`, and `src/local_cache.rs:1443-1624`. Each performs: NUL-delimited `ls-files`, a concurrent stdin write to `check-attr`, split on `\0`, `chunks_exact(3)`, validate `filter` / `lfs`, then a path-safety check with its own `#[cfg(unix)] OsStringExt` / `#[cfg(not(unix))] from_utf8` pair. Three copies of `git_path_bytes_to_path_buf`, three of the containment check.

**Gain:** ~200 lines, one thorough test suite instead of three partial ones, and one place to maintain the non-UTF-8 and Windows path handling that already has three `Learnings` entries. **Drawback:** The three invocations differ in real ways — `local_cache` uses `--cached` with `--git-dir` and `--work-tree`, `migration` uses `--source=<commit>` with input batching. The shared piece is the parse and path-safety half, not the invocation. Extracting only that is the safe scope.

## 7. Mechanical duplication

### 7.1 Google Drive URL builders: seven copies of the same block

`drive_file_metadata_url`, `drive_object_metadata_url`, `drive_shard_folder_metadata_url`, `drive_object_lookup_url`, `drive_shard_folder_lookup_url`, `drive_file_create_url`, `drive_media_download_url`, and `drive_resumable_upload_url` (`src/google_drive.rs:2319-2642`, ~320 lines). Each repeats: blank-ID check, `drive_api_base_path_already_targets_drive_api`, `path_segments_mut` with an identical error, `pop_if_empty`, then `if already_targets { ... } else { extend(["drive", "v3", ...]) }`. They diverge only in the `fields=` query.

**Gain:** ~200 lines. One helper `fn drive_files_url(base, extra_segments) -> StorageResult<Url>` plus per-endpoint query parameters. The `/drive/v3` suffix rule has its own `Learnings` entry and is currently enforced in eight places. As provider-internal cleanup, a tidier Drive module is also a better template for the next storage provider. **Drawback:** `drive_resumable_upload_url` is the outlier — it pops two segments and prepends `upload` — so it remains partly special-cased.

### 7.2 Two Drive URL validators agreeing on four of five rules

`validate_drive_api_base_url` (`src/google_drive.rs:2261`) and `validate_drive_resumable_upload_session_url` (`src/google_drive.rs:2644`) both check scheme, loopback HTTP, credentials, and fragment, with the same `StorageError::Upstream { provider: "google_drive", status: None, ... }` boilerplate repeated eleven times between them.

**Gain:** ~90 lines, plus a `drive_config_error(msg)` helper removes roughly 20 more instances of that four-line error literal across the file. **Drawback:** The query-string rules genuinely differ (the base rejects, the session URL allows), so a small policy flag is needed — mirroring the `HttpUrlPolicy` pattern `src/http_transport.rs` already uses well.

### 7.3 `metadata.rs` boilerplate

- Five `_async` twins (`lookup_object_async`, `mark_object_stale_async`, `record_verified_object_async`, `start_transfer_attempt_async`, `finish_transfer_attempt_async`) are each entirely `Arc::clone`, `spawn_blocking`, `map_err(MetadataTaskJoin)`. One generic `async fn blocking<T>(self: &Arc<Self>, f: impl FnOnce(&Self) -> ServerResult<T> + Send + 'static)` replaces all five bodies.
- `run_migrations` (`src/metadata.rs:573-607`) has five identical `if version < N { execute_batch(SQL).map_err(...) }` blocks. Iterate a `[(u32, &str)]` slice instead.
- `.map_err(|source| ServerError::MetadataOperation { path: self.path.clone(), source })` appears roughly 20 times. One `fn op_err(&self)` closure factory removes it.

**Gain:** ~120 lines, and adding migration 6 becomes a one-line array entry instead of a copy-paste. The async-boundary test recorded in `[tests] SQLite lock timing` still applies, since it tests the blocking-pool dispatch, not each individual method. **Drawback:** The generic `blocking` needs `Self: Send + Sync + 'static` bounds. Straightforward, but the closure signature reads slightly less obviously than the current spelled-out version.

### 7.4 Small cross-file duplicates

- `command_status_text`: three copies (`src/git.rs:652`, `src/migration.rs:4184`, `src/credentials.rs:1575`) plus `process_status_text` (`src/cli.rs:3829`), all the same four lines.
- `truncated_lossy_message`: identical in `src/git.rs:712` and `src/migration.rs:4191`.
- The `is_char_boundary`-walking stderr truncator: `src/cli.rs:3836` and `src/credentials.rs:1049`.
- `read_google_response_body` (`src/google_drive.rs:3500`) and `read_github_error_body` (`src/github_auth.rs:881`): the same bounded 16 KiB chunk reader.
- `src/git.rs`: `git_stdout`, `git_config_get`, and `run_git_config` are three copies of spawn, status check, size cap, UTF-8 decode.
- `src/migration.rs`: `run_git`, `run_git_os`, `run_git_os_with_limit`, `run_git_os_vec`, and `run_git_os_vec_with_limit` are five wrappers differing only in argument type and limit.

**Constraint from the plugin goal:** the Drive and GitHub response readers must be shared through a neutral module alongside `src/http_transport.rs`. A `google_drive.rs` to `github_auth.rs` dependency in either direction would be worse than the duplication.

**Gain:** Individually small, collectively ~150 lines, and it removes the "which copy is canonical?" question. **Drawback:** Crosses module boundaries with different error enums, so it needs a shared module plus a generic error mapper. Low risk, moderate churn.

### 7.5 Duplicated error-mapping tables in `server.rs`

`git_lfs_storage_error_response_parts` (`src/server.rs:3072`) and `lfs_batch_object_error_from_server_error` (`src/server.rs:3149`) match the same nine `StorageError` variants to an HTTP status and an LFS batch code respectively, in two separate ~50-line tables. Adding a variant means remembering both.

Additionally, the category match inside `finish_failed_transfer_attempt` (`src/server.rs:2206-2210`) is character-for-character identical to `server_error_log_category` (`src/server.rs:2216-2220`). Call the function.

**Gain:** ~50 lines and one fewer synchronization hazard. This also matters for extensibility: a new provider only needs to produce the right `StorageError` variant, and both mappings follow. **Drawback:** The messages differ slightly by design ("Git LFS object was not found" vs "object not found"), so the unified entry needs both fields.

### 7.6 Duplicated batch response builders

`download_batch_response_with_storage_lookup` and `upload_batch_response_with_storage_lookup` (`src/server.rs:2901-2977`) share identical dedupe, `buffered` lookup, and reorder-by-request logic, differing only in `LfsBatchDownloadObject` vs `LfsBatchUploadObject`. `outcome_object` and `upload_outcome_object` are both eight-line variant-collapsing matches.

**Gain:** ~60 lines via one generic over a small trait or an `impl Fn(LfsObject, Option<&StoredObject>) -> T` mapper. **Drawback:** Generics over two unrelated enums require a trait, which may read slightly worse than the honest duplication. Marginal call.

## 8. Dead public API

| Item | Status | Action |
| --- | --- | --- |
| `fetch_authenticated_github_user` (`src/github_auth.rs:707`) | Zero callers; only re-exported at `src/lib.rs:38` | Remove — GitHub-specific convenience, not a seam |
| `GoogleDriveRootValidator::with_client` (`src/google_drive.rs:492`) | Zero callers; every test uses `with_client_and_api_base_url` | Remove — strict subset of the used constructor |
| `lfs_server_router` (`src/server.rs:347`) | Zero callers; only re-exported at `src/lib.rs:95` | Keep — plausible embedder entry point; fold into §9.2 |

**Note on scope:** because providers may eventually become separate crates, the `src/lib.rs` re-export block is the plugin surface. Do not trim it wholesale. Remove only items verified as both unused and not an extension point.

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
