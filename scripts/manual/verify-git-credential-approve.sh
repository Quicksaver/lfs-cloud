#!/usr/bin/env bash
set -euo pipefail

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
lfs_url="https://lfs.example.invalid/github.com/owner/repo.git/info/lfs"
other_lfs_url="https://lfs.example.invalid/github.com/owner/other.git/info/lfs"
token="manual-lfs-token"

GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.https://lfs.example.invalid/.useHttpPath true

printf 'url=%s\nusername=lfs-cloud\npassword=%s\n\n' "$lfs_url" "$token" |
  GIT_CONFIG_GLOBAL="$global_config" \
    git -c "credential.helper=store --file=$store_file" \
      credential approve

approved="$(
  printf 'url=%s\n\n' "$lfs_url" |
    GIT_CONFIG_GLOBAL="$global_config" \
      git -c "credential.helper=store --file=$store_file" \
        credential fill
)"

printf '%s\n' "$approved" | grep -Fx 'protocol=https' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'host=lfs.example.invalid' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'path=github.com/owner/repo.git/info/lfs' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'username=lfs-cloud' >/dev/null
printf '%s\n' "$approved" | grep -Fx "password=$token" >/dev/null

if printf 'url=%s\n\n' "$other_lfs_url" |
  GIT_CONFIG_GLOBAL="$global_config" \
    git -c "credential.helper=store --file=$store_file" \
      credential fill >/dev/null 2>&1; then
  echo "unexpectedly retrieved repo-scoped token for a different LFS URL" >&2
  exit 1
fi

echo "git credential approve stored and retrieved a path-scoped LFS Cloud token"
