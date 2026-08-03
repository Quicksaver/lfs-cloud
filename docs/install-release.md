# Install, Build, And Release Shape

LFS Cloud is currently a single Rust package with a library target and a small CLI binary. Its local release tooling produces checksummed platform archives, Debian packages, and direct installers, then publishes Homebrew and WinGet metadata plus optional APT metadata from an interactive maintainer command. No release package is signed or notarized yet, and none of these channels will resolve until the first release is published.

## Prerequisites

- Rust toolchain compatible with the crate's `rust-version`.
- Git. Historical migration scans with `lfscloud migrate --ref ...` or `--all-refs` require Git 2.40.0 or newer because they evaluate attributes with `git check-attr --source`. Current-checkout dry-run planning does not use that option.
- Git LFS for `lfscloud pull` and migration source-fetch steps. Read-only migration planning can still report repository config without fetching, but fetching missing source objects depends on `git lfs fetch`.
- Python 3 for the manual and aggregate smoke verifiers.
- PowerShell 7 (`pwsh`) for the native Windows verifier.
- `cargo-audit` 0.22.2 for local RustSec scans. Install the same pinned version used by CI with `cargo install cargo-audit --locked --version 0.22.2`.
- Node.js/Yarn for repository formatting checks and the one-shot smoke runner.

The final local publisher additionally requires:

- Homebrew, for formula validation and publication to the configured tap.
- the authenticated Cloudsmith CLI only when opting into Debian repository uploads with `LFS_CLOUD_APT_CLOUDSMITH_TARGET`.
- GitHub CLI access with repository administration permission to enable release immutability, plus permission to publish releases, push the Homebrew tap and WinGet fork, and open WinGet pull requests.

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

The orchestrator requires a clean tracked worktree and proves that the checked-out commit is exactly the current branch on `origin`. Its terminal display reports one status line per environment without streaming the child processes. Complete stdout and stderr are written to separate `logs/verify-[timestamp]/[environment].log` files, and each run removes verification log files older than 14 days. The orchestrator waits for every selected verifier even when one fails and prints the corresponding log path with each result. Each verifier packages its verified binary, checksum, and commit-bound build manifest under `dist/`, then posts its own `local-checks/*` commit status through the authenticated GitHub CLI.

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

Linux verification disables Cargo incremental compilation because the persistent target volumes already retain reusable dependency and final build artifacts; retaining per-edit incremental compiler sessions adds several gigabytes per architecture without helping the pushed-commit verification workflow.

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

### All-In-One Fleet Release

The preferred manual entry point performs the complete cross-machine release and publication flow:

```bash
yarn release:all patch
# or: yarn release:all minor
# or: yarn release:all major
```

Run it from a clean local `main` checkout that exactly matches `origin/main`. The Mac needs `ssh`, `iconv`, and `base64`. The command connects in batch mode to the fleet SSH alias `windows-desktop`, so that alias must already authenticate non-interactively with a key or agent and without a password or passphrase prompt. It requires a clean Windows `main` checkout at `E:\Projects\lfs-cloud` and fast-forwards that checkout to the same `origin/main` commit. Override those fleet defaults only when the device configuration has intentionally changed:

```bash
export LFS_CLOUD_WINDOWS_SSH_HOST=windows-desktop
export LFS_CLOUD_WINDOWS_REPO='E:\Projects\lfs-cloud'
```

Before changing the version, the coordinator reads trusted commit statuses and runs only missing macOS ARM64, Linux ARM64 Docker, Linux x86-64 Docker, and native Windows x86-64 checks. It then creates the version commit, tag, and asset-less draft; fast-forwards Windows to the version commit; and runs the three local version checks concurrently with the exact tagged Windows draft continuation. Complete output for each cross-machine wave is retained under `logs/release-[timestamp]-[stage]-[pid]/`, with coordinator logs older than 14 days removed by `release_all_prune_logs`, while the nested local verifier logs remain under `logs/verify-[timestamp]/`.

After all four trusted version statuses are green, the coordinator attaches the macOS and Linux assets, invokes publication for that exact tag without an interactive selector, and follows the established immutable GitHub release, direct installer, Homebrew, optional APT, and WinGet distribution flow. A failed phase stops later phases and terminates its still-running cross-machine peer. Rerun the same command after correcting the failure: a current `Release vX.Y.Z` commit resumes its existing draft or incomplete immutable distribution instead of incrementing again.

The lower-level commands below remain available for manual recovery and individual phases.

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
4. Creates and pushes the annotated `vX.Y.Z` tag, then creates or refreshes an asset-less draft GitHub release using every `CHANGELOG.md` section newer than the highest successfully published stable release, through the candidate version, as its description. Failed, interrupted, or unpublished draft versions therefore keep rolling into later release notes until a release is published.
5. Reruns all three deterministic local verifiers, writing their output to `logs/verify-[timestamp]/[environment].log`, and requires their new commit statuses to be green.
6. Verifies that the packaged macOS binary reports the new version and that every platform archive's SHA-256 checksum and build manifest match the exact commit.
7. Uploads the three platform archives, both Debian packages, direct installers, checksums, and commit-bound manifests, verifies those draft assets, and leaves the release editable for the Windows continuation.

As soon as `release.sh` creates the draft, continue the same release from a clean native Windows x86-64 checkout; the Windows verifier can run while the macOS and Linux verifiers are still working:

```powershell
yarn release:windows
```

The Windows continuation inspects draft semantic-version releases and offers an arrow-key menu containing only versions whose latest `local-checks/windows-x86_64` status is not successful. Use Up/Down to highlight a version and Enter to select it; Escape cancels. Missing, failed, or interrupted checks remain selectable so completion can be retried. For the selected version, it verifies that the local and remote tag identify the draft release commit, checks out the tag in detached-HEAD mode, and runs the complete native Windows verifier against that exact source version.

After verification, the continuation uploads the versioned Windows ZIP archive, SHA-256 checksum, and commit-bound build manifest to the existing draft. It validates each remote asset's name, size, and GitHub-reported SHA-256 digest before recording the Windows status as successful. A build or upload failure records a failed status and leaves the version eligible for a retry. The original branch and commit are restored before the command exits, including after failures. It never publishes the GitHub release.

If an interruption occurs after the version commit or tag is pushed, do not increment the version again. Restore the required green local status and artifact if necessary, then continue safely:

```bash
yarn release:local resume
```

Running `major`, `minor`, or `patch` again from an untagged `Release vX.Y.Z` commit also resumes that version automatically. If `HEAD` is already tagged as the current version, the release command refuses another increment until a new commit exists, preventing an accidental consecutive version-only release.

## Final Publication And Distribution

Run final publication locally rather than through GitHub Actions. On macOS with Bash, Homebrew, Git, and GitHub CLI available, invoke:

```bash
# Optional; defaults to Quicksaver/homebrew-tap
export LFS_CLOUD_HOMEBREW_TAP_REPO=OWNER/homebrew-TAP
# Optional; enables Cloudsmith APT publication when set
export LFS_CLOUD_APT_CLOUDSMITH_TARGET=OWNER/REPOSITORY/DISTRO/VERSION
yarn release:publish
```

The publisher lists semantic draft releases only when the latest macOS ARM64, Linux x86-64, Linux ARM64, and Windows x86-64 statuses are all successful and were created by the authenticated GitHub user. Its arrow-key selector also lists already-published immutable releases whose configured distribution statuses are incomplete, allowing a failed channel to be resumed without editing the release. When `LFS_CLOUD_APT_CLOUDSMITH_TARGET` is unset, the selector reports `apt:skipped`, Cloudsmith is not required, and APT does not participate in release completion.

The current checkout may contain staged, unstaged, or untracked work. Publication uses the selected release's remote tag, trusted verification statuses, and downloaded GitHub assets rather than rebuilding or publishing files from the worktree.

After selection, the command downloads every draft asset and revalidates its GitHub-reported digest, checksum, build manifest, version, target, architecture, and commit. It generates the Homebrew formula and WinGet manifests from those verified bytes, then enables immutable releases for the repository and publishes the selected draft as the latest immutable release without another confirmation prompt. It verifies GitHub's generated release attestation before distributing anything. All release assets must therefore be present before this point; published release tags and assets cannot be replaced. Use Escape in the release selector to cancel before selecting a version.

Distribution proceeds independently and records one commit status per channel:

| Status | Publication |
| --- | --- |
| `distribution/direct-installer` | Downloads the now-public shell and PowerShell installer assets and verifies their digests. |
| `distribution/homebrew` | Trusts the configured tap locally, validates the generated formula, commits it to `Formula/lfscloud.rb`, and pushes the tap. A matching generated formula left by a failed validation is resumed safely. |
| `distribution/apt` | When `LFS_CLOUD_APT_CLOUDSMITH_TARGET` is set, uploads the `amd64` and `arm64` Debian packages to that Cloudsmith distribution; otherwise skipped. |
| `distribution/winget-submitted` | Pushes schema-declared portable manifests to the authenticated user's `winget-pkgs` fork and opens or updates the upstream pull request; fork cloning leaves upstream configuration to the publisher. |

A channel failure does not make the immutable GitHub release editable. Rerun `yarn release:publish`; successful statuses are skipped and only incomplete configured channels require their corresponding local tool and configuration. If Cloudsmith is configured later, an immutable release without `distribution/apt` becomes eligible for resumption so its existing Debian packages can be published. WinGet's status means the Community repository pull request was submitted, not that Microsoft has merged it.

No GitHub Actions workflow creates or publishes these releases or distribution entries.

## Installing A Published Release

Direct installation and later updates use the same command. The installer resolves the latest release unless given a pinned semantic version, verifies the archive checksum and executable-reported version, and writes an ownership receipt beside the installed binary. It refuses to replace a binary owned by another installation method unless explicitly forced.

macOS ARM64 or Linux x86-64/ARM64:

```bash
curl -fsSL https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.sh | sh

# Pin a version or choose another directory:
curl -fsSL https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.sh \
  | sh -s -- --version 1.2.3 --install-dir "$HOME/.local/bin"
```

Windows x86-64:

```powershell
irm https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.ps1 | iex

# Pin a version while retaining the script for inspection:
irm https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.ps1 -OutFile install-lfscloud.ps1
./install-lfscloud.ps1 -Version 1.2.3 -InstallDir "$HOME/.local/bin"
```

Neither installer elevates privileges or edits `PATH`; the default destination is `~/.local/bin`. Use `--force` or `-Force` only when deliberately replacing an unmanaged executable.

Once the corresponding repository entry exists, package-manager installation is:

```bash
brew install Quicksaver/tap/lfscloud
```

```powershell
winget install --exact --id Quicksaver.LFSCloud
```

If the release maintainer opted into Cloudsmith publication, first follow the generated setup instructions for the configured Debian repository, then run:

```bash
sudo apt update
sudo apt install lfscloud
```

## Expected Release Artifact

Successful manual CI jobs package tested executables in 14-day workflow artifacts. The first local release phase places tested macOS ARM64, Linux ARM64 musl, and Linux x86-64 musl archives, Debian packages, direct installers, SHA-256 checksums, and commit-bound build manifests in a GitHub draft. The native Windows continuation adds its tested Windows x86-64 archive, checksum, and manifest. The expected release artifact remains the compiled `lfscloud` binary plus documentation for:

- supported platform and architecture
- config file schema
- GitHub PAT setup
- Google Drive credential setup
- manual verification scripts that were run
- known MVP limitations

The current repository still does not define binary signatures, macOS notarization, Cargo registry publication, or automated CI release publishing. Homebrew, optional APT, WinGet, and direct-installer publication are deliberately initiated by the local interactive publisher.

## Licensing

LFS Cloud source is licensed under the [MIT License](../LICENSE). The Rust and repository-tooling package metadata use the matching SPDX identifier. The `publish = false` and `private: true` settings remain intentional until release packaging exists; they control registry publication rather than source-code license rights.

The current locked Rust dependency graph declares only permissive license terms, or multi-license expressions with a permissive option. Prettier, the only JavaScript development dependency, is also MIT-licensed and is not part of the compiled artifact. Before distributing a release, regenerate the dependency inventory from the final lockfiles and preserve any notices required by dependencies whose terms include Apache-2.0, BSD, ISC, Unicode, CDLA-Permissive, or other notice-bearing licenses.

The deterministic local verifiers run the complete verification set before assembling the draft:

```bash
yarn verify:all
```
