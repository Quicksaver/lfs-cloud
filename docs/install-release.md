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
- Node.js/Yarn only for repository formatting checks.

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
```

GitHub Actions runs the Rust formatting, lint, build, unit/integration, and
documentation-test gates natively on Linux, macOS, and Windows. The test suite
uses platform-native child processes for timeout and process-tree cleanup
coverage, so Windows exercises the same recursive termination boundary as Unix
rather than only compiling it.

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

Until packaging is implemented, the expected artifact is the compiled
`lfscloud` binary plus documentation for:

- supported platform and architecture
- config file schema
- GitHub OAuth setup
- Google Drive credential setup
- manual verification scripts that were run
- known MVP limitations

The current repository does not yet define:

- packaged archives
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
cargo build
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
cargo audit
yarn lint:check
```
