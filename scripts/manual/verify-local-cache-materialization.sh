#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

cargo test --manifest-path "$project_dir/Cargo.toml" --lib local_cache::tests::materialize
cargo test --manifest-path "$project_dir/Cargo.toml" --lib local_cache::tests::hydrate_pointer

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "non-macOS platform: verified fallback materialization tests; skipped /bin/cp -c probe"
  exit 0
fi

source_file="$tmp_dir/source.bin"
clone_file="$tmp_dir/clone.bin"
printf 'lfs-cloud copy-on-write materialization probe\n' >"$source_file"

/bin/cp -c "$source_file" "$clone_file"
cmp "$source_file" "$clone_file" >/dev/null

echo "macOS /bin/cp -c materialization primitive verified"
