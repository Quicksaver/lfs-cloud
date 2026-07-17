#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

repo_dir="$tmp_dir/repo"
store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
lfs_url="https://lfs.example.invalid/github.com/owner/repo.git/info/lfs"
other_lfs_url="https://lfs.example.invalid/github.com/owner/other.git/info/lfs"
token="manual-lfs-token"

mkdir -p "$repo_dir"
git -C "$repo_dir" init >/dev/null
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git
git -C "$repo_dir" config --local \
  credential.https://lfs.example.invalid/.useHttpPath false

GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.helper "store --file=$store_file"

(
  cd "$repo_dir"
  printf '%s\n' "$token" |
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
      cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- \
        login --server https://lfs.example.invalid --no-open
) >"$tmp_dir/login-output"

grep -F "https://lfs.example.invalid/auth/github/login" "$tmp_dir/login-output" >/dev/null
grep -F "stored local LFS credential" "$tmp_dir/login-output" >/dev/null
if grep -F "$token" "$tmp_dir/login-output" >/dev/null; then
  echo "login output leaked the local LFS token" >&2
  exit 1
fi
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git -C "$repo_dir" config --local --get \
    credential.https://lfs.example.invalid/.useHttpPath |
  grep -Fx 'true' >/dev/null

approved="$(
  printf 'url=%s\n\n' "$lfs_url" |
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
      git -C "$repo_dir" credential fill
)"

printf '%s\n' "$approved" | grep -Fx 'protocol=https' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'host=lfs.example.invalid' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'path=github.com/owner/repo.git/info/lfs' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'username=lfs-cloud' >/dev/null
printf '%s\n' "$approved" | grep -Fx "password=$token" >/dev/null

other_approved="$(
  {
    printf 'url=%s\n\n' "$other_lfs_url" |
      GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
        git -C "$repo_dir" credential fill
  } 2>/dev/null || true
)"
if printf '%s\n' "$other_approved" | grep -Fx "password=$token" >/dev/null; then
  echo "unexpectedly retrieved repo-scoped token for a different LFS URL" >&2
  exit 1
fi

echo "lfs-cloud login stored a path-scoped local LFS token"
