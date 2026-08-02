#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$project_dir/scripts/lib/lfscloud-command.sh"
# shellcheck source=../lib/python.sh
source "$project_dir/scripts/lib/python.sh"
tmp_dir="$(mktemp -d)"
server_pid=""
trap 'if [[ -n "${server_pid:-}" ]]; then kill "$server_pid" >/dev/null 2>&1 || true; fi; rm -rf "$tmp_dir"' EXIT

repo_dir="$tmp_dir/repo"
cache_root="$tmp_dir/cache"
store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
port_file="$tmp_dir/server.port"
token="manual-status-lfs-token"
python_bin="$(lfscloud_find_python3 || true)"

if [[ -z "$python_bin" ]]; then
  echo "Python 3 is required to run the manual status verifier" >&2
  exit 1
fi

"$python_bin" - "$port_file" <<'PY' &
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
config_file="$tmp_dir/lfscloud.yml"

gcloud_config_dir="$tmp_dir/gcloud-drive"
mkdir -p "$repo_dir" "$cache_root/objects" "$gcloud_config_dir"
printf '{}\n' >"$gcloud_config_dir/application_default_credentials.json"
gcloud_config_path="$gcloud_config_dir"
if command -v cygpath >/dev/null 2>&1; then
  gcloud_config_path="$(cygpath -m "$gcloud_config_dir")"
fi
git -C "$repo_dir" init >/dev/null
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git

cat >"$config_file" <<YAML
server:
  host: 127.0.0.1
  port: $port
  public_url: $server_url
  session_encryption_secret: status-smoke-session-secret-at-least-32-characters

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com

storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: $gcloud_config_path
      executable: git
    root_folder_id: root-folder

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
YAML

GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.helper "store --file=$store_file"
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global "credential.$server_url.useHttpPath" true
printf 'protocol=http\nhost=127.0.0.1:%s\npath=github.com/owner/repo.git/info/lfs\nusername=lfscloud\npassword=%s\n\n' \
  "$port" "$token" |
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    git credential approve

if ! (
  cd "$repo_dir"
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    run_lfscloud "$project_dir" --config "$config_file" status --cache-root "$cache_root"
) >"$tmp_dir/status-output" 2>&1; then
  cat "$tmp_dir/status-output" >&2
  exit 1
fi

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

echo "lfscloud status verified repository, server, auth, storage, and cache checks"
