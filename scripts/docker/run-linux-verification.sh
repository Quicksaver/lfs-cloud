#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/release-common.sh
source "$SCRIPT_DIR/../lib/release-common.sh"

if (($# != 3)); then
  printf 'usage: run-linux-verification.sh <rust-target> <artifact-platform> <container-arch>\n' >&2
  exit 2
fi

rust_target="$1"
artifact_platform="$2"
expected_container_arch="$3"
repo_root="/workspace"
start_sha="$(git -C "$repo_root" rev-parse HEAD)"
package_stage=""

case "$artifact_platform" in
  linux-arm64-musl)
    ui_prefix="[linux-arm64]"
    ;;
  linux-x86_64-musl)
    ui_prefix="[linux-x86-64]"
    ;;
  *)
    ui_prefix="[linux]"
    ;;
esac

release_ui_initialize "$ui_prefix" "Verify $artifact_platform release"
finalize_linux_runner() {
  local exit_code=$?

  trap - EXIT
  if [[ -n "$package_stage" ]]; then
    rm -rf -- "$package_stage"
  fi
  release_ui_finalize
  exit "$exit_code"
}
trap finalize_linux_runner EXIT

mkdir -p "${HOME:?}"

if [[ "$(uname -s)" != "Linux" ]] || [[ "$(uname -m)" != "$expected_container_arch" ]]; then
  release_die \
    "Expected Linux/$expected_container_arch, found $(uname -s)/$(uname -m)"
fi

release_run_step \
  "Configure the container repository" \
  git config --global --add safe.directory "$repo_root"
if ! git -C "$repo_root" diff --quiet --ignore-submodules -- \
  || ! git -C "$repo_root" diff --cached --quiet --ignore-submodules --; then
  release_die "Tracked working-tree changes appeared before Linux verification"
fi

cd "$repo_root"
export CARGO_BUILD_TARGET="$rust_target"
export CARGO_TARGET_DIR="/target"
export CARGO_TERM_COLOR="never"
export LFS_CLOUD_SMOKE_BINARY="$CARGO_TARGET_DIR/$rust_target/release/lfscloud"
export LFS_CLOUD_SMOKE_SKIP_CARGO_TESTS="1"
export LFS_CLOUD_SMOKE_THROWAWAY="/tmp/lfscloud-smoke-root"
target_env="${rust_target//-/_}"
export "CARGO_TARGET_${target_env^^}_LINKER=musl-gcc"
export "CC_${target_env}=musl-gcc"

rm -rf -- "$LFS_CLOUD_SMOKE_THROWAWAY"
mkdir -p "$LFS_CLOUD_SMOKE_THROWAWAY"
git init --quiet "$LFS_CLOUD_SMOKE_THROWAWAY"

release_run_step "Install repository tooling" yarn install --immutable
release_run_step "Verify Git LFS" git lfs version
release_run_step "Configure Git LFS" git lfs install --skip-repo
release_run_step "Check Rust formatting" cargo fmt --all -- --check
release_run_step "Check Rust lints" cargo clippy --all-targets -- -D warnings
release_run_step \
  "Run automated Rust tests" \
  cargo test --all-targets -- --test-threads=1
release_run_step "Run Rust documentation tests" cargo test --doc
release_run_step "Build the release binary" cargo build --release
release_run_step "Audit locked Rust dependencies" cargo audit
release_run_step "Check repository formatting" yarn lint:check
release_run_step \
  "Run smoke tests against the exact release binary" \
  node \
  --no-warnings \
  --experimental-strip-types \
  .agents/skills/smoke-test/scripts/smoke-test.ts

version="$(
  awk '
    /^\[package\]$/ { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^version = "[^"]+"$/ {
      value = $0
      sub(/^version = "/, "", value)
      sub(/"$/, "", value)
      print value
      exit
    }
  ' Cargo.toml
)"
if [[ "$("$LFS_CLOUD_SMOKE_BINARY" --version)" != "lfscloud $version" ]]; then
  release_die "Release binary version does not match package version $version"
fi

artifact_name="lfscloud-v${version}-${artifact_platform}"
artifact="$repo_root/dist/$artifact_name.tar.gz"
manifest="$repo_root/dist/$artifact_name.build.json"

package_linux_artifact() {
  local artifact_digest

  package_stage="$(mktemp -d /tmp/lfscloud-linux-package.XXXXXX)"
  mkdir -p "$repo_root/dist" "$package_stage/$artifact_name/docs"
  rm -f -- "$artifact" "$artifact.sha256" "$manifest"
  cp "$LFS_CLOUD_SMOKE_BINARY" "$package_stage/$artifact_name/lfscloud"
  cp LICENSE "$package_stage/$artifact_name/LICENSE"
  cp README.md "$package_stage/$artifact_name/README.md"
  cp docs/configuration.md "$package_stage/$artifact_name/docs/configuration.md"
  cp docs/install-release.md "$package_stage/$artifact_name/docs/install-release.md"
  tar \
    --create \
    --gzip \
    --file "$artifact" \
    --directory "$package_stage" \
    --group=0 \
    --mtime='@0' \
    --numeric-owner \
    --owner=0 \
    --sort=name \
    "$artifact_name"
  (
    cd "$(dirname "$artifact")"
    sha256sum "$(basename "$artifact")" > "$(basename "$artifact").sha256"
  )

  artifact_digest="$(sha256sum "$artifact" | awk 'NR == 1 { print $1 }')"
  jq -n \
    --arg artifact "$(basename "$artifact")" \
    --arg commit "$start_sha" \
    --arg container_arch "$(uname -m)" \
    --arg digest "$artifact_digest" \
    --arg kernel "$(uname -sr)" \
    --arg rustc "$(rustc --version)" \
    --arg target "$rust_target" \
    --arg version "$version" \
    '{
      schema_version: 1,
      artifact: $artifact,
      commit: $commit,
      version: $version,
      target: $target,
      container_arch: $container_arch,
      kernel: $kernel,
      rustc: $rustc,
      sha256: $digest
    }' > "$manifest"
  rm -rf -- "$package_stage"
  package_stage=""
}
release_run_step "Package the verified Linux release binary" package_linux_artifact

if [[ "$(git rev-parse HEAD)" != "$start_sha" ]] \
  || ! git diff --quiet --ignore-submodules -- \
  || ! git diff --cached --quiet --ignore-submodules --; then
  release_die "Tracked source changed during Linux verification"
fi

release_pass "Linux Docker verification passed for $start_sha"
release_info "Artifact: $artifact"
release_info "Checksum: $artifact.sha256"
release_info "Build manifest: $manifest"
