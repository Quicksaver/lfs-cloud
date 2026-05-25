#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'kill "$server_pid" >/dev/null 2>&1 || true; rm -rf "$tmp_dir"' EXIT

repo_dir="$tmp_dir/repo"
cache_root="$tmp_dir/cache"
store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
port_file="$tmp_dir/server.port"
token="manual-status-lfs-token"

python3 - "$port_file" <<'PY' &
import socket
import sys

port_file = sys.argv[1]
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(5)
with open(port_file, "w", encoding="utf-8") as handle:
    handle.write(str(listener.getsockname()[1]))
while True:
    connection, _ = listener.accept()
    connection.close()
PY
server_pid="$!"

for _ in $(seq 1 50); do
  if [[ -s "$port_file" ]]; then
    break
  fi
  sleep 0.1
done
if [[ ! -s "$port_file" ]]; then
  echo "test TCP server did not report a port" >&2
  exit 1
fi

port="$(cat "$port_file")"
server_url="http://127.0.0.1:$port"
lfs_url="$server_url/github.com/owner/repo.git/info/lfs"
config_file="$tmp_dir/lfs-cloud.yml"

mkdir -p "$repo_dir" "$cache_root/objects"
git -C "$repo_dir" init >/dev/null
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git

cat >"$config_file" <<YAML
server:
  host: 127.0.0.1
  port: $port
  public_url: $server_url

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-user-a
    root_folder_id: root-folder

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
YAML

GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.helper "store --file=$store_file"
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global "credential.$server_url.useHttpPath" true
printf 'protocol=http\nhost=127.0.0.1:%s\npath=github.com/owner/repo.git/info/lfs\nusername=lfs-cloud\npassword=%s\n\n' \
  "$port" "$token" |
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    git credential approve

export LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_DRIVE_USER_A='{"client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh-token"}'

(
  cd "$repo_dir"
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- \
      --config "$config_file" status --cache-root "$cache_root"
) >"$tmp_dir/status-output"

grep -F "config     ok" "$tmp_dir/status-output" >/dev/null
grep -F "repository ok" "$tmp_dir/status-output" >/dev/null
grep -F "server     ok" "$tmp_dir/status-output" >/dev/null
grep -F "route      ok      $lfs_url" "$tmp_dir/status-output" >/dev/null
grep -F "mapping    ok      github-main:owner/repo -> drive-user-a" "$tmp_dir/status-output" >/dev/null
grep -F "auth       ok      local LFS credential found" "$tmp_dir/status-output" >/dev/null
grep -F "storage    ok      google_drive drive-user-a credential is configured" "$tmp_dir/status-output" >/dev/null
grep -F "cache      ok" "$tmp_dir/status-output" >/dev/null
if grep -F "$token" "$tmp_dir/status-output" >/dev/null; then
  echo "status output leaked the local LFS token" >&2
  exit 1
fi

echo "lfs-cloud status verified repository, server, auth, storage, and cache checks"
