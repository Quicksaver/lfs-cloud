#!/usr/bin/env bash

set -euo pipefail

if (($# != 3)); then
  printf 'usage: run-linux-verification.sh <rust-target> <artifact-platform> <container-arch>\n' >&2
  exit 2
fi

rust_target="$1"
artifact_platform="$2"
expected_container_arch="$3"
repo_root="/workspace"
start_sha="$(git -C "$repo_root" rev-parse HEAD)"

mkdir -p "${HOME:?}"

if [[ "$(uname -s)" != "Linux" ]] || [[ "$(uname -m)" != "$expected_container_arch" ]]; then
  printf 'error: expected Linux/%s, found %s/%s\n' \
    "$expected_container_arch" \
    "$(uname -s)" \
    "$(uname -m)" >&2
  exit 1
fi

git config --global --add safe.directory "$repo_root"
if ! git -C "$repo_root" diff --quiet --ignore-submodules -- \
  || ! git -C "$repo_root" diff --cached --quiet --ignore-submodules --; then
  printf 'error: tracked working-tree changes appeared before Linux verification\n' >&2
  exit 1
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

printf '==> Install repository tooling\n'
yarn install --immutable
git lfs version
git lfs install --skip-repo

printf '==> Check Rust formatting\n'
cargo fmt --all -- --check

printf '==> Check Rust lints\n'
cargo clippy --all-targets -- -D warnings

printf '==> Run automated Rust tests\n'
cargo test --all-targets -- --test-threads=1

printf '==> Run Rust documentation tests\n'
cargo test --doc

printf '==> Build the release binary\n'
cargo build --release

printf '==> Audit locked Rust dependencies\n'
cargo audit

printf '==> Check repository formatting\n'
yarn lint:check

printf '==> Run smoke tests against the exact release binary\n'
node --no-warnings --experimental-strip-types \
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
  printf 'error: release binary version does not match package version %s\n' "$version" >&2
  exit 1
fi

artifact_name="lfscloud-v${version}-${artifact_platform}"
artifact="$repo_root/dist/$artifact_name.tar.gz"
manifest="$repo_root/dist/$artifact_name.build.json"
package_stage="$(mktemp -d /tmp/lfscloud-linux-package.XXXXXX)"
trap 'rm -rf -- "$package_stage"' EXIT

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

if [[ "$(git rev-parse HEAD)" != "$start_sha" ]] \
  || ! git diff --quiet --ignore-submodules -- \
  || ! git diff --cached --quiet --ignore-submodules --; then
  printf 'error: tracked source changed during Linux verification\n' >&2
  exit 1
fi

printf 'PASS: Linux Docker verification passed for %s\n' "$start_sha"
printf 'Artifact: %s\n' "$artifact"
printf 'Checksum: %s\n' "$artifact.sha256"
printf 'Build manifest: %s\n' "$manifest"
