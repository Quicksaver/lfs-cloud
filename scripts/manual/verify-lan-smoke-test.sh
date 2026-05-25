#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
server_pid=""
trap 'if [[ -n "${server_pid:-}" ]]; then kill "$server_pid" >/dev/null 2>&1 || true; fi; rm -rf "$tmp_dir"' EXIT

python_bin="$(command -v python3 || command -v python || true)"

if [[ -z "$python_bin" ]] || ! "$python_bin" - <<'PY'
import sys

sys.exit(0 if sys.version_info[0] >= 3 else 1)
PY
then
  echo "Python 3 is required to run the LAN smoke verifier" >&2
  exit 1
fi

if [[ -n "${LFS_CLOUD_LAN_PORT:-}" ]]; then
  port="$LFS_CLOUD_LAN_PORT"
else
  port="$("$python_bin" - <<'PY'
import socket

with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)"
fi

host="${LFS_CLOUD_LAN_HOST:-0.0.0.0}"
public_url="${LFS_CLOUD_LAN_PUBLIC_URL:-http://127.0.0.1:$port}"
config_file="${LFS_CLOUD_LAN_CONFIG:-$tmp_dir/lfs-cloud-lan.yml}"
route_host="${LFS_CLOUD_LAN_ROUTE_HOST:-github.com}"
route_owner="${LFS_CLOUD_LAN_ROUTE_OWNER:-owner}"
route_repo="${LFS_CLOUD_LAN_ROUTE_REPO:-repo}"
route_path="${LFS_CLOUD_LAN_ROUTE_PATH:-/$route_host/$route_owner/$route_repo.git/info/lfs}"
startup_timeout_seconds="${LFS_CLOUD_LAN_STARTUP_TIMEOUT_SECONDS:-20}"

if [[ "$public_url" == */ ]]; then
  echo "LFS_CLOUD_LAN_PUBLIC_URL must not end with a trailing slash" >&2
  exit 1
fi

if [[ ! "$port" =~ ^[0-9]+$ ]] || [[ "$port" -eq 0 ]] || [[ "$port" -gt 65535 ]]; then
  echo "LFS_CLOUD_LAN_PORT must be a TCP port from 1 to 65535" >&2
  exit 1
fi

if [[ ! "$startup_timeout_seconds" =~ ^[0-9]+$ ]] || [[ "$startup_timeout_seconds" -eq 0 ]]; then
  echo "LFS_CLOUD_LAN_STARTUP_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 1
fi

for route_part in "$route_host" "$route_owner" "$route_repo"; do
  if [[ -z "$route_part" ]] || [[ "$route_part" == *"/"* ]]; then
    echo "LFS_CLOUD_LAN_ROUTE_HOST, LFS_CLOUD_LAN_ROUTE_OWNER, and LFS_CLOUD_LAN_ROUTE_REPO must be non-empty path segments" >&2
    exit 1
  fi
done

if [[ "$route_path" != /* ]] || [[ "$route_path" == *"?"* ]] || [[ "$route_path" == *"#"* ]]; then
  echo "LFS_CLOUD_LAN_ROUTE_PATH must be an absolute path without query or fragment" >&2
  exit 1
fi

if [[ -z "${LFS_CLOUD_LAN_CONFIG:-}" ]]; then
  cat >"$config_file" <<YAML
server:
  host: 127.0.0.1
  port: $port
  public_url: $public_url
  metadata_path: $tmp_dir/metadata.sqlite3

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: lan-smoke-client
    oauth_client_secret: lan-smoke-secret

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-user-a
    root_folder_id: lan-smoke-root

repositories:
  - id: github-main:$route_owner/$route_repo
    repo_provider: github-main
    host: $route_host
    owner: $route_owner
    name: $route_repo
    storage_provider: drive-user-a
YAML
elif [[ -n "${LFS_CLOUD_LAN_PUBLIC_URL:-}" ]]; then
  echo "Using existing LFS_CLOUD_LAN_CONFIG; ensure server.public_url already matches $public_url" >&2
fi

server_log="$tmp_dir/server.log"

cargo build --quiet --manifest-path "$project_dir/Cargo.toml"

cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- \
  --config "$config_file" serve --host "$host" --port "$port" \
  >"$server_log" 2>&1 &
server_pid="$!"

startup_attempts=$((startup_timeout_seconds * 4))

for _ in $(seq 1 "$startup_attempts"); do
  if grep -F "lfs-cloud server running" "$server_log" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$server_pid" >/dev/null 2>&1; then
    cat "$server_log" >&2
    echo "lfs-cloud serve exited before reporting startup" >&2
    exit 1
  fi
  sleep 0.25
done

if ! grep -F "lfs-cloud server running" "$server_log" >/dev/null; then
  cat "$server_log" >&2
  echo "lfs-cloud serve did not report startup before timeout" >&2
  exit 1
fi

grep -F "local:   http://127.0.0.1:$port" "$server_log" >/dev/null
grep -F "network: " "$server_log" >/dev/null

if command -v curl >/dev/null 2>&1; then
  http_status="$(
    curl --silent --show-error \
      --output "$tmp_dir/info-response" \
      --write-out "%{http_code}" \
      "http://127.0.0.1:$port$route_path"
  )"
  if [[ "$http_status" != "401" ]]; then
    cat "$tmp_dir/info-response" >&2
    echo "expected unauthenticated LFS info request to return HTTP 401, got $http_status" >&2
    exit 1
  fi
  grep -F '"message":"LFS Cloud authentication required"' "$tmp_dir/info-response" >/dev/null
fi

cat <<CHECKLIST
LAN smoke server preflight passed.

Manual cross-machine checklist:

1. On the server machine, choose a reachable LAN base URL with no trailing slash:
   LFS_CLOUD_LAN_PUBLIC_URL=http://<server-lan-ip>:$port \\
     LFS_CLOUD_LAN_PORT=$port \\
     scripts/manual/verify-lan-smoke-test.sh
   For a real disposable-repo transfer run, point LFS_CLOUD_LAN_CONFIG at the
   real server config, make sure its server.public_url uses that same LAN URL,
   and set LFS_CLOUD_LAN_ROUTE_HOST, LFS_CLOUD_LAN_ROUTE_OWNER, and
   LFS_CLOUD_LAN_ROUTE_REPO to a mapped repository if it is not github.com/owner/repo.
2. Confirm the server output includes both:
   local:   http://127.0.0.1:$port
   network: http://<server-lan-ip>:$port
3. From a second machine on the same trusted LAN, verify the route boundary:
   curl -i http://<server-lan-ip>:$port$route_path
   Expected: HTTP 401, Git LFS JSON content, and a Basic or Bearer auth challenge.
4. With a disposable GitHub repo mapped in the real server config, run from the
   client worktree:
   lfs-cloud init --server http://<server-lan-ip>:$port
   lfs-cloud login --server http://<server-lan-ip>:$port
   git lfs env
   Expected: the Git LFS endpoint points at the LAN URL for the disposable repo.
5. Track and push one small Git LFS file from the client, then clone or pull
   from another clean worktree using the same LAN URL.
   Expected: upload and download batch requests succeed, object bytes round-trip
   through Google Drive storage, and no Drive URL or OAuth token appears in CLI,
   server, or Git LFS output.
6. Stop the server and remove the disposable GitHub repo, Drive folder contents,
   and local credential-helper entries created for the smoke test.
CHECKLIST
