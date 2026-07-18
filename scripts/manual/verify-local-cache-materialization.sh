#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

cargo test --manifest-path "$project_dir/Cargo.toml" --lib local_cache::tests::materialize
cargo test --manifest-path "$project_dir/Cargo.toml" --lib local_cache::tests::hydrate_pointer

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "non-macOS platform: verified fallback materialization tests; skipped APFS clone assertion"
  exit 0
fi

cargo test --manifest-path "$project_dir/Cargo.toml" --lib \
  local_cache::tests::materialize_object_uses_copy_on_write_on_apfs -- --exact

echo "macOS APFS copy-on-write materialization verified"
