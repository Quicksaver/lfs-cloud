#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/release-common.sh
source "$SCRIPT_DIR/../lib/release-common.sh"

usage() {
  cat <<'EOF'
Usage: ./scripts/local/verify-macos.sh

Run the complete deterministic macOS verification with the active system Rust
toolchain and post the local-checks/macos-arm64 status to the pushed commit.
EOF
}

if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi
if (($# != 0)); then
  usage >&2
  exit 2
fi

release_initialize "$SCRIPT_DIR"
cd "$RELEASE_REPO_ROOT"

if [[ "$(uname -s)" != "Darwin" ]] || [[ "$(uname -m)" != "arm64" ]]; then
  release_die "Local macOS verification requires an arm64 Mac."
fi
macos_version="$(sw_vers -productVersion)"

release_require_tracked_clean
release_require_current_commit_on_origin

status_started=0
checks_passed=0
status_finalized=0
package_stage=""

finalize_local_status() {
  local exit_code=$?

  trap - EXIT
  if [[ -n "$package_stage" ]]; then
    rm -rf -- "$package_stage"
  fi
  if ((status_started == 1 && checks_passed == 0 && status_finalized == 0)); then
    release_post_status \
      "$RELEASE_SHA" \
      "$LOCAL_MACOS_STATUS_CONTEXT" \
      "failure" \
      "Local macOS $macos_version arm64 checks failed" \
      || printf 'warning: failed to record the local failure status\n' >&2
  fi
  exit "$exit_code"
}
trap finalize_local_status EXIT

release_info "Record local macOS verification as pending"
release_post_status \
  "$RELEASE_SHA" \
  "$LOCAL_MACOS_STATUS_CONTEXT" \
  "pending" \
  "Local macOS $macos_version arm64 checks are running"
status_started=1

release_require_command cargo
release_require_command node
release_require_command yarn
release_require_command shasum
release_require_command tar

export CARGO_BUILD_TARGET="aarch64-apple-darwin"
export CARGO_TERM_COLOR="never"

release_info "Install repository tooling"
yarn install --immutable
git lfs version

release_info "Check Rust formatting"
cargo fmt --all -- --check

release_info "Check Rust lints"
cargo clippy --all-targets -- -D warnings

release_info "Run automated Rust tests"
cargo test --all-targets -- --test-threads=1

release_info "Run Rust documentation tests"
cargo test --doc

release_info "Build the release binary"
cargo build --release

if ! cargo audit --version 2>/dev/null | grep -Eq '(^|[[:space:]])0\.22\.2($|[[:space:]])'; then
  release_info "Install cargo-audit 0.22.2"
  cargo install cargo-audit --locked --version 0.22.2
fi

release_info "Audit locked Rust dependencies"
cargo audit

release_info "Check repository formatting"
yarn lint:check

release_binary="$RELEASE_REPO_ROOT/target/$CARGO_BUILD_TARGET/release/lfscloud"
release_info "Run smoke tests against the exact release binary"
LFS_CLOUD_SMOKE_BINARY="$release_binary" \
  LFS_CLOUD_SMOKE_SKIP_CARGO_TESTS="1" \
  node --no-warnings --experimental-strip-types \
  .agents/skills/smoke-test/scripts/smoke-test.ts

version="$(release_require_matching_versions)"
if [[ "$("$release_binary" --version)" != "lfscloud $version" ]]; then
  release_die "Release binary version does not match package version $version."
fi

artifact="$(release_macos_artifact_path "$version")"
manifest="$(release_macos_manifest_path "$version")"
mkdir -p "$(dirname "$artifact")"
rm -f -- "$artifact" "$artifact.sha256" "$manifest"

release_info "Package the verified macOS release binary"
package_name="lfscloud-v$version-macos-arm64"
package_stage="$(mktemp -d "$RELEASE_REPO_ROOT/dist/.package.XXXXXX")"
mkdir -p "$package_stage/$package_name/docs"
cp "$release_binary" "$package_stage/$package_name/lfscloud"
cp "$RELEASE_REPO_ROOT/LICENSE" "$package_stage/$package_name/LICENSE"
cp "$RELEASE_REPO_ROOT/README.md" "$package_stage/$package_name/README.md"
cp "$RELEASE_REPO_ROOT/docs/configuration.md" "$package_stage/$package_name/docs/configuration.md"
cp "$RELEASE_REPO_ROOT/docs/install-release.md" "$package_stage/$package_name/docs/install-release.md"
COPYFILE_DISABLE=1 tar -czf "$artifact" -C "$package_stage" "$package_name"
rm -rf -- "$package_stage"
package_stage=""
(
  cd "$(dirname "$artifact")"
  shasum -a 256 "$(basename "$artifact")" > "$(basename "$artifact").sha256"
)
release_verify_checksum "$artifact"
artifact_digest="$(shasum -a 256 "$artifact" | awk 'NR == 1 { print $1 }')"
jq -n \
  --arg artifact "$(basename "$artifact")" \
  --arg commit "$RELEASE_SHA" \
  --arg digest "$artifact_digest" \
  --arg macos "$macos_version" \
  --arg rustc "$(rustc --version)" \
  --arg target "$CARGO_BUILD_TARGET" \
  --arg version "$version" \
  '{
    schema_version: 1,
    artifact: $artifact,
    commit: $commit,
    version: $version,
    target: $target,
    macos: $macos,
    rustc: $rustc,
    sha256: $digest
  }' > "$manifest"
release_verify_macos_manifest "$artifact" "$manifest" "$version" "$RELEASE_SHA"

checks_passed=1
release_info "Record local macOS verification as successful"
release_post_status \
  "$RELEASE_SHA" \
  "$LOCAL_MACOS_STATUS_CONTEXT" \
  "success" \
  "Local macOS $macos_version arm64 checks passed"
status_finalized=1
trap - EXIT

release_pass "Local macOS verification passed for $RELEASE_SHA"
printf 'Artifact: %s\n' "$artifact"
printf 'Checksum: %s\n' "$artifact.sha256"
printf 'Build manifest: %s\n' "$manifest"
