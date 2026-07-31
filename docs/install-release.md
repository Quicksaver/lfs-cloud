# Install, Build, And Release Shape

LFS Cloud is currently a single Rust package with a library target and a small CLI binary. There is no published package, installer, or signed release artifact yet.

## Prerequisites

- Rust toolchain compatible with the crate's `rust-version`.
- Git. Historical migration scans with `lfscloud migrate --ref ...` or `--all-refs` require Git 2.40.0 or newer because they evaluate attributes with `git check-attr --source`. Current-checkout dry-run planning does not use that option.
- Git LFS for `lfscloud pull` and migration source-fetch steps. Read-only migration planning can still report repository config without fetching, but fetching missing source objects depends on `git lfs fetch`.
- Python 3 for the manual and aggregate smoke verifiers.
- PowerShell 7 (`pwsh`) for the native Windows verifier.
- `cargo-audit` 0.22.2 for local RustSec scans. Install the same pinned version used by CI with `cargo install cargo-audit --locked --version 0.22.2`.
- Node.js/Yarn for repository formatting checks and the one-shot smoke runner.

For real GitHub and Google Drive operation you also need:

- A GitHub PAT restricted to the repositories served by LFS Cloud.
- Google Cloud CLI installed for the account running the server.
- Google ADC generated with a Desktop OAuth client and Drive scope.
- A Drive folder accessible to the OAuth client with `drive.file` scope.
- A Git credential helper configured for local token storage.

## Local Development Build

```bash
cargo build
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
cargo audit
yarn lint:check
node --no-warnings --experimental-strip-types .agents/skills/smoke-test/scripts/smoke-test.ts
```

The local smoke runner creates an isolated child repository under `~/Sites/throwaway` on macOS and `~/Projects/throwaway` on Windows. The selected parent must already be a Git repository and remains unchanged; set `LFS_CLOUD_SMOKE_THROWAWAY` to use another prepared parent.

On a Windows x86-64 machine, test the native verification automation with:

```powershell
yarn test:verify-windows
```

After pushing a candidate commit, run every deterministic local verifier supported by the current system in parallel:

```bash
yarn verify:all
```

The orchestrator detects host capabilities before repository or GitHub checks. On macOS it selects the macOS ARM64 verifier; on Windows it selects the Windows x86-64 verifier. Whenever a responsive Docker Linux engine is available, it also selects both Linux Docker verifiers. The selected checks run concurrently. If the host supports no verifier, the command fails before contacting GitHub.

The orchestrator requires a clean tracked worktree and proves that the checked-out commit is exactly the current branch on `origin`. Its terminal display keeps a bounded rolling output window for each environment, waits for every selected verifier even when one fails, and reports their results independently. Each verifier packages its verified binary, checksum, and commit-bound build manifest under `dist/`, then posts its own `local-checks/*` commit status through the authenticated GitHub CLI.

Run one environment independently when needed:

```bash
yarn verify:macos
yarn verify:linux-arm64
yarn verify:linux-x86-64
```

Run the equivalent native Windows verification independently from a Windows x86-64 checkout:

```powershell
yarn verify:windows
```

The macOS verifier first requires a Darwin ARM64 host, then uses the active system Rust toolchain to run formatting, Clippy, all Cargo targets, documentation tests, the pinned RustSec audit, repository formatting, and smoke tests against the exact release executable. Its status is `local-checks/macos-arm64`. The Windows verifier requires a Windows x86-64 host and enforces the same active-toolchain checks, clean pushed-commit boundary, exact release-binary smoke tests, and artifact integrity checks. It writes a Windows ZIP archive, checksum, and commit-bound build manifest under `dist/`; its status is `local-checks/windows-x86_64`. The Linux scripts first require a responsive Docker Linux engine whose active Buildx builder advertises the requested platform, then enforce the same clean, pushed commit boundary and build the exact `package.rust-version` toolchain inside a parameterized image. Their distinct statuses are `local-checks/linux-arm64-docker` and `local-checks/linux-x86_64-docker`. Platform preflights run before GitHub authentication, origin checks, commit-status writes, image builds, or verification commands.

`yarn verify:all` selects from all four verifiers. Run an individual `yarn verify:*` command when only one environment should be checked.

Docker resources are deliberately stable and persist after a check:

| Linux check | Image | Container | Target volume |
| --- | --- | --- | --- |
| x86-64 musl | `lfscloud-checks-linux-x86-64:local` | `lfscloud-checks-linux-x86-64` | `lfscloud-checks-linux-x86-64-target` |
| ARM64 musl | `lfscloud-checks-linux-arm64:local` | `lfscloud-checks-linux-arm64` | `lfscloud-checks-linux-arm64-target` |

Both containers share the architecture-independent `lfscloud-checks-cargo-cache` registry volume. Source is bind-mounted from the current checkout, while compiled targets stay isolated from macOS and the other Linux architecture. Repeated runs rebuild the named image with Docker's cache and restart the matching stopped container rather than creating anonymous images, containers, or volumes. If `.env.local` points at readable Google Drive ADC configuration, the same directory is mounted into the container so eligible live-provider smoke checks remain enabled.

GitHub Actions `CI` is manual-only and installs the toolchain declared by `package.rust-version` in `Cargo.toml`, including the matrix target and required components. Local verification continues to use the active system Rust toolchain. One workflow dispatch runs formatting, linting, all Cargo targets, documentation tests, release builds, and smoke tests for all four native targets:

| Artifact                     | Rust target                  | Runner architecture |
| ---------------------------- | ---------------------------- | ------------------- |
| `lfscloud-windows-x86_64`    | `x86_64-pc-windows-msvc`     | Windows 2025 x64    |
| `lfscloud-macos-arm64`       | `aarch64-apple-darwin`       | macOS 26 ARM64      |
| `lfscloud-linux-x86_64-musl` | `x86_64-unknown-linux-musl`  | Ubuntu 26.04 x64    |
| `lfscloud-linux-arm64-musl`  | `aarch64-unknown-linux-musl` | Ubuntu 26.04 ARM64  |

Each matrix job runs the target's Cargo and documentation tests before its optimized build. The smoke harness then receives the exact release executable, so CLI, local-cache, credential, migration, and eligible live-provider checks cannot silently fall back to a separately built debug binary. The opt-in LAN check honors the same executable boundary when explicitly enabled. The test suite uses platform-native child processes for timeout and process-tree cleanup coverage, so Windows exercises the same recursive termination boundary as Unix.

Manual CI runs live GitHub and Google Drive checks when their repository secrets are configured. The disposable-resource PAT needs classic `repo` and `delete_repo` scopes. CI writes the ADC JSON to an isolated `application_default_credentials.json`, installs `gcloud`, and gives the smoke runner only `LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR`, which points to the containing isolated gcloud configuration directory. Local smoke runs set the same directory-path variable in `.env.local`.

Rotate the local smoke-test values and matching GitHub repository secrets with:

```bash
yarn rotate:github
yarn rotate:google-drive
yarn rotate:slack
```

The GitHub script reads hidden terminal input. The Google Drive script accepts the isolated gcloud configuration directory, updates the ignored root `.env.local`, and syncs its generated ADC file contents to the repository's `LFS_CLOUD_GOOGLE_DRIVE_ADC_JSON` secret. The Slack script reads a hidden, channel-bound incoming webhook URL, updates `SLACK_WEBHOOK_URL` in `.env.local`, and syncs the matching GitHub Actions repository secret. All three scripts resolve the target repository from the `origin` remote.

The `Slack workflow failure notifications` workflow watches the `CI` and `Dependency audit` workflows and posts only their failed runs. Its notification job maps the `SLACK_WEBHOOK_URL` repository secret into the process environment; the webhook value is never committed. A manual workflow dispatch sends a test notification without deliberately breaking CI.

The separate dependency-audit workflow installs the repository-pinned `cargo-audit` version and fails when the locked Rust graph contains an applicable RustSec vulnerability. It runs for dependency changes on pull requests and `main`, and on a weekly schedule so a newly published advisory is detected even when the lockfile has not changed. Informational warnings remain visible in the job output and require maintainer triage, but do not fail the audit unless they apply to code the project uses.

Repository maintainers own both advisory remediation and updates to the pinned `cargo-audit` version. A failing advisory audit blocks merge. Prefer updating the affected direct requirement or the smallest transitive lockfile package, then run the full verification set below. An advisory may be ignored only when the repository documents why the affected code is unreachable, links a tracking issue, and records a review or expiry date; there are currently no ignored advisories.

Run the CLI from the workspace:

```bash
cargo run -- --help
cargo run -- serve --config ./lfscloud.yml
```

## Release Build

Build the optimized binary:

```bash
cargo build --release
./target/release/lfscloud --help
```

For a local PATH install from this checkout:

```bash
cargo install --path .
```

## Local Release

Authenticate `gh` with permission to write commit statuses, tags, and releases, then run all local verifiers on the current pushed commit:

```bash
yarn verify:all
```

Create the next semantic version with exactly one increment:

```bash
yarn release:local patch
# or: yarn release:local minor
# or: yarn release:local major
```

The release script:

1. Requires a completely clean worktree and a current commit exactly matching the current branch on `origin`.
2. Requires the latest macOS, Linux ARM64 Docker, and Linux x86-64 Docker statuses to be successful and created by the currently authenticated GitHub user.
3. Updates the matching versions in `Cargo.toml`, `Cargo.lock`, and `package.json`; moves the current `CHANGELOG.md` entries from `Unreleased` into a dated `[X.Y.Z] - YYYY-MM-DD` section; adds a fresh `Unreleased` section; then commits `Release vX.Y.Z` and pushes that commit without force.
4. Reruns all three deterministic local verifiers and requires their new commit statuses to be green.
5. Verifies that the packaged macOS binary reports the new version and that every platform archive's SHA-256 checksum and build manifest match the exact commit.
6. Creates and pushes an annotated `vX.Y.Z` tag for the exact verified commit.
7. Creates a draft GitHub release using the matching `CHANGELOG.md` section as its description, uploads the three binary archives, checksums, and build manifests, verifies all assets, and only then publishes the release. Resuming an existing draft refreshes its description from that same committed section.

After `release.sh` succeeds, continue the same published release from a clean native Windows x86-64 checkout:

```powershell
yarn release:windows
```

The Windows continuation inspects published semantic-version releases and offers an arrow-key menu containing only versions whose latest `local-checks/windows-x86_64` status is not successful. Use Up/Down to highlight a version and Enter to select it; Escape cancels. Missing, failed, or interrupted checks remain selectable so publication can be retried. For the selected version, it verifies that the local and remote tag identify the published release commit, checks out the tag in detached-HEAD mode, and runs the complete native Windows verifier against that exact source version.

After verification, the continuation uploads the versioned Windows ZIP archive, SHA-256 checksum, and commit-bound build manifest to the existing GitHub release. It validates each remote asset's name, size, and GitHub-reported SHA-256 digest before recording the Windows status as successful. A build or publication failure records a failed status and leaves the version eligible for a retry. The original branch and commit are restored before the command exits, including after failures.

If an interruption occurs after the version commit or tag is pushed, do not increment the version again. Restore the required green local status and artifact if necessary, then continue safely:

```bash
yarn release:local resume
```

## Expected Release Artifact

Successful manual CI jobs package tested executables in 14-day workflow artifacts. The first local release phase publishes tested macOS ARM64, Linux ARM64 musl, and Linux x86-64 musl archives with SHA-256 checksums and commit-bound build manifests as GitHub Release assets. The native Windows continuation adds the corresponding tested Windows x86-64 archive, checksum, and manifest to that published version. The expected release artifact remains the compiled `lfscloud` binary plus documentation for:

- supported platform and architecture
- config file schema
- GitHub PAT setup
- Google Drive credential setup
- manual verification scripts that were run
- known MVP limitations

The current repository still does not define:

- installer scripts
- binary signatures or macOS notarization
- automated CI release publishing
- Homebrew, Cargo registry, or OS package distribution

## Licensing

LFS Cloud source is licensed under the [MIT License](../LICENSE). The Rust and repository-tooling package metadata use the matching SPDX identifier. The `publish = false` and `private: true` settings remain intentional until release packaging exists; they control registry publication rather than source-code license rights.

The current locked Rust dependency graph declares only permissive license terms, or multi-license expressions with a permissive option. Prettier, the only JavaScript development dependency, is also MIT-licensed and is not part of the compiled artifact. Before distributing a release, regenerate the dependency inventory from the final lockfiles and preserve any notices required by dependencies whose terms include Apache-2.0, BSD, ISC, Unicode, CDLA-Permissive, or other notice-bearing licenses.

The deterministic local verifiers run the complete verification set before publishing:

```bash
yarn verify:all
```
