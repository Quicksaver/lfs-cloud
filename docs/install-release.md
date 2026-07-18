# Install, Build, And Release Shape

LFS Cloud is currently a single Rust package with a library target and a small
CLI binary. There is no published package, installer, or signed release artifact
yet.

## Prerequisites

- Rust toolchain compatible with the crate's `rust-version`.
- Git. Historical migration scans with `lfs-cloud migrate --ref ...` or
  `--all-refs` require Git 2.40.0 or newer because they evaluate attributes with
  `git check-attr --source`. Current-checkout planning does not use that option.
- Git LFS for `lfs-cloud pull` and migration source-fetch steps. Read-only
  migration planning can still report repository config without fetching, but
  fetching missing source objects depends on `git lfs fetch`.
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
yarn lint:check
```

Run the CLI from the workspace:

```bash
cargo run -- --help
cargo run -- serve --config ./lfs-cloud.yml
```

## Release Build

Build the optimized binary:

```bash
cargo build --release
./target/release/lfs-cloud --help
```

For a local PATH install from this checkout:

```bash
cargo install --path .
```

## Expected Release Artifact

Until packaging is implemented, the expected artifact is the compiled
`lfs-cloud` binary plus documentation for:

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

Release automation should run the full verification set before publishing:

```bash
cargo fmt --check
cargo build
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
yarn lint:check
```
