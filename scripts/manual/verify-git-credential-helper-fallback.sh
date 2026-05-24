#!/usr/bin/env bash
set -euo pipefail

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

global_config="$tmp_dir/gitconfig"

if GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --get-urlmatch credential.helper \
    https://lfs.example.com/github.com/owner/repo.git/info/lfs >/dev/null 2>&1; then
  echo "expected isolated Git config to have no credential helper" >&2
  exit 1
fi

# The substring filter intentionally covers the credential-helper fallback unit
# tests and the approval tests that guard the missing-helper path.
cargo test --lib credential_helper

echo "credential-helper fallback unit tests verified"
