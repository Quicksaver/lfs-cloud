# Install, Build, And Release Shape

LFS Cloud is currently a single Rust package with a library target and a small
CLI binary. There is no published package, installer, or signed release artifact
yet.

## Prerequisites

- Rust toolchain compatible with the crate's `rust-version`.
- Git. Historical migration scans with `lfscloud migrate --ref ...` or
  `--all-refs` require Git 2.40.0 or newer because they evaluate attributes with
  `git check-attr --source`. Current-checkout planning does not use that option.
- Git LFS for `lfscloud pull` and migration source-fetch steps. Read-only
  migration planning can still report repository config without fetching, but
  fetching missing source objects depends on `git lfs fetch`.
- `cargo-audit` 0.22.2 for local RustSec scans. Install the same pinned version
  used by CI with
  `cargo install cargo-audit --locked --version 0.22.2`.
- Node.js/Yarn for repository formatting checks and the one-shot smoke runner.

For real GitHub and Google Drive operation you also need:

- A GitHub OAuth app with callback URL matching the running server.
- Google OAuth credentials with a refresh token for the Drive account.
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

GitHub Actions runs formatting, linting, all Cargo targets, documentation tests,
release builds, and smoke tests for four native targets:

| Artifact                     | Rust target                  | Runner architecture |
| ---------------------------- | ---------------------------- | ------------------- |
| `lfscloud-windows-x86_64`    | `x86_64-pc-windows-msvc`     | Windows 2025 x64    |
| `lfscloud-macos-arm64`       | `aarch64-apple-darwin`       | macOS 26 ARM64      |
| `lfscloud-linux-x86_64-musl` | `x86_64-unknown-linux-musl`  | Ubuntu 26.04 x64    |
| `lfscloud-linux-arm64-musl`  | `aarch64-unknown-linux-musl` | Ubuntu 26.04 ARM64  |

Each matrix job runs the target's Cargo and documentation tests before its
optimized build. The smoke harness then receives the exact release executable,
so CLI, local-cache, credential, migration, and eligible live-provider checks
cannot silently fall back to a separately built debug binary. The opt-in LAN
check honors the same executable boundary when explicitly enabled. The test
suite uses platform-native child processes for timeout and process-tree cleanup
coverage, so Windows exercises the same recursive termination boundary as Unix.

Pull requests run the complete local smoke coverage without repository secrets.
On pushes and manual runs, the GitHub, Google Drive, and black-box Git LFS checks
also run when `LFS_CLOUD_GITHUB_TOKEN` and the three Google Drive OAuth values
`LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID`, `LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET`, and
`LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN` are configured as repository secrets.
The live-test boundary combines those values into the provider credential JSON;
CI does not require one composite JSON secret.

Rotate the local smoke-test values and matching GitHub repository secrets with:

```bash
yarn rotate:github
yarn rotate:google-drive
```

The scripts read hidden terminal input, update the existing keys in the ignored
root `.env.local`, and sync them to the repository resolved from the `origin`
remote. Leaving a prompt empty retains and re-syncs its current local value.

The separate dependency-audit workflow installs the repository-pinned
`cargo-audit` version and fails when the locked Rust graph contains an
applicable RustSec vulnerability. It runs for dependency changes on pull
requests and `main`, and on a weekly schedule so a newly published advisory is
detected even when the lockfile has not changed. Informational warnings remain
visible in the job output and require maintainer triage, but do not fail the
audit unless they apply to code the project uses.

Repository maintainers own both advisory remediation and updates to the pinned
`cargo-audit` version. A failing advisory audit blocks merge. Prefer updating
the affected direct requirement or the smallest transitive lockfile package,
then run the full verification set below. An advisory may be ignored only when
the repository documents why the affected code is unreachable, links a
tracking issue, and records a review or expiry date; there are currently no
ignored advisories.

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

## Expected Release Artifact

Successful CI jobs package the tested executable in a 14-day workflow artifact.
These archives are CI outputs, not signed or published releases. The expected
release artifact remains the compiled `lfscloud` binary plus documentation for:

- supported platform and architecture
- config file schema
- GitHub OAuth setup
- Google Drive credential setup
- manual verification scripts that were run
- known MVP limitations

The current repository still does not define:

- installer scripts
- checksums or signatures
- CI release publishing
- Homebrew, Cargo registry, or OS package distribution

## Licensing

LFS Cloud source is licensed under the [MIT License](../LICENSE). The Rust and
repository-tooling package metadata use the matching SPDX identifier. The
`publish = false` and `private: true` settings remain intentional until release
packaging exists; they control registry publication rather than source-code
license rights.

The current locked Rust dependency graph declares only permissive license
terms, or multi-license expressions with a permissive option. Prettier, the
only JavaScript development dependency, is also MIT-licensed and is not part of
the compiled artifact. Before distributing a release, regenerate the
dependency inventory from the final lockfiles and preserve any notices
required by dependencies whose terms include Apache-2.0, BSD, ISC, Unicode,
CDLA-Permissive, or other notice-bearing licenses.

Release automation should run the full verification set before publishing:

```bash
cargo fmt --check
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
cargo build --release
node --no-warnings --experimental-strip-types .agents/skills/smoke-test/scripts/smoke-test.ts
cargo audit
yarn lint:check
```
