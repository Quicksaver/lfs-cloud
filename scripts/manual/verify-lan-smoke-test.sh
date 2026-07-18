#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t lfs-cloud-lan)"
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

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to verify the unauthenticated LFS HTTP route" >&2
  exit 1
fi

host="${LFS_CLOUD_LAN_HOST:-0.0.0.0}"

if [[ -n "${LFS_CLOUD_LAN_PORT:-}" ]]; then
  port="$LFS_CLOUD_LAN_PORT"
else
  port="$("$python_bin" - "$host" <<'PY'
import socket
import sys

host = sys.argv[1]
infos = socket.getaddrinfo(host, 0, type=socket.SOCK_STREAM)
last_error = None

for family, socktype, proto, _, sockaddr in infos:
    try:
        with socket.socket(family, socktype, proto) as listener:
            listener.bind(sockaddr)
            print(listener.getsockname()[1])
            sys.exit(0)
    except OSError as error:
        last_error = error

raise SystemExit(f"failed to choose a free port for {host}: {last_error}")
PY
)"
fi

public_url="${LFS_CLOUD_LAN_PUBLIC_URL:-http://127.0.0.1:$port}"
config_file="${LFS_CLOUD_LAN_CONFIG:-$tmp_dir/lfs-cloud-lan.yml}"
route_host="${LFS_CLOUD_LAN_ROUTE_HOST:-github.com}"
route_owner="${LFS_CLOUD_LAN_ROUTE_OWNER:-owner}"
route_repo="${LFS_CLOUD_LAN_ROUTE_REPO:-repo}"
route_path="${LFS_CLOUD_LAN_ROUTE_PATH:-/$route_host/$route_owner/$route_repo.git/info/lfs}"
startup_timeout_seconds="${LFS_CLOUD_LAN_STARTUP_TIMEOUT_SECONDS:-20}"

is_loopback_host() {
  case "$1" in
    127.* | localhost | ::1 | "[::1]")
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_unspecified_host() {
  case "$1" in
    0.0.0.0 | :: | "[::]")
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

advertised_local_url() {
  "$python_bin" - "$1" "$2" <<'PY'
import ipaddress
import sys

host = sys.argv[1]
port = sys.argv[2]

if host in {"0.0.0.0", "::", "[::]"}:
    host = "127.0.0.1"
else:
    try:
        ip = ipaddress.ip_address(host.strip("[]"))
    except ValueError:
        pass
    else:
        if ip.version == 6:
            host = f"[{ip}]"
        else:
            host = str(ip)

print(f"http://{host}:{port}")
PY
}

require_file_contains() {
  local pattern="$1"
  local file="$2"
  local message="$3"

  if ! grep -F "$pattern" "$file" >/dev/null; then
    echo "$message" >&2
    cat "$file" >&2
    exit 1
  fi
}

require_file_matches() {
  local pattern="$1"
  local file="$2"
  local message="$3"

  if ! grep -E "$pattern" "$file" >/dev/null; then
    echo "$message" >&2
    cat "$file" >&2
    exit 1
  fi
}

build_lfs_cloud_binary() {
  cargo build --quiet --manifest-path "$project_dir/Cargo.toml" --message-format=json |
    "$python_bin" -c '
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    target = message.get("target") or {}
    if (
        message.get("reason") == "compiler-artifact"
        and target.get("name") == "lfs-cloud"
        and "bin" in target.get("kind", [])
        and message.get("executable")
    ):
        print(message["executable"])
        sys.exit(0)

raise SystemExit("Cargo did not report a built lfs-cloud executable")
'
}

extract_server_public_url() {
  "$python_bin" - "$1" <<'PY'
import sys

path = sys.argv[1]
in_server = False
server_indent = None

with open(path, encoding="utf-8") as handle:
    for raw_line in handle:
        line = raw_line.rstrip("\n")
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue

        indent = len(line) - len(line.lstrip(" "))
        if stripped == "server:":
            in_server = True
            server_indent = indent
            continue

        if in_server and indent <= server_indent:
            break

        if in_server and stripped.startswith("public_url:"):
            value = stripped.split(":", 1)[1].strip()
            if (value.startswith('"') and value.endswith('"')) or (
                value.startswith("'") and value.endswith("'")
            ):
                value = value[1:-1]
            print(value)
            sys.exit(0)

sys.exit(1)
PY
}

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
  if [[ -z "$route_part" ]] || [[ ! "$route_part" =~ ^[A-Za-z0-9._~-]+$ ]]; then
    echo "LFS_CLOUD_LAN_ROUTE_HOST, LFS_CLOUD_LAN_ROUTE_OWNER, and LFS_CLOUD_LAN_ROUTE_REPO must be non-empty URL-safe path segments" >&2
    exit 1
  fi
done

if [[ "$route_path" != /* ]] || [[ "$route_path" == *"?"* ]] || [[ "$route_path" == *"#"* ]] || [[ "$route_path" =~ [[:space:]] ]]; then
  echo "LFS_CLOUD_LAN_ROUTE_PATH must be an absolute path without whitespace, query, or fragment" >&2
  exit 1
fi

if ! is_loopback_host "$host"; then
  echo "Notice: lfs-cloud will bind to $host and may be reachable from your LAN." >&2
fi

if [[ -z "${LFS_CLOUD_LAN_CONFIG:-}" ]]; then
  cat >"$config_file" <<YAML
server:
  host: $host
  port: $port
  public_url: $public_url
  allow_insecure_http: true
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
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
YAML
elif [[ -n "${LFS_CLOUD_LAN_PUBLIC_URL:-}" ]]; then
  actual_public_url="$(extract_server_public_url "$config_file" || true)"
  if [[ "$actual_public_url" != "$public_url" ]]; then
    echo "LFS_CLOUD_LAN_CONFIG server.public_url must match LFS_CLOUD_LAN_PUBLIC_URL ($public_url)" >&2
    exit 1
  fi
fi

server_log="$tmp_dir/server.log"
expected_local_url="$(advertised_local_url "$host" "$port")"

lfs_cloud_bin="$(build_lfs_cloud_binary)"

"$lfs_cloud_bin" --config "$config_file" serve --host "$host" --port "$port" \
  >"$server_log" 2>&1 &
server_pid="$!"

startup_attempts=$((startup_timeout_seconds * 4))
startup_http_status=""

for _ in $(seq 1 "$startup_attempts"); do
  startup_http_status="$(
    curl --silent --show-error \
      --output "$tmp_dir/startup-response" \
      --write-out "%{http_code}" \
      "$expected_local_url$route_path" 2>"$tmp_dir/startup-curl-error" || true
  )"
  if [[ "$startup_http_status" == "401" ]]; then
    break
  fi
  if ! kill -0 "$server_pid" >/dev/null 2>&1; then
    cat "$server_log" >&2
    cat "$tmp_dir/startup-curl-error" >&2
    echo "lfs-cloud serve exited before reporting startup" >&2
    exit 1
  fi
  sleep 0.25
done

if [[ "$startup_http_status" != "401" ]]; then
  cat "$server_log" >&2
  cat "$tmp_dir/startup-curl-error" >&2
  echo "lfs-cloud serve did not respond with HTTP 401 before timeout" >&2
  exit 1
fi

local_line="$(grep -E "local:[[:space:]]+" "$server_log" | tail -n 1 || true)"
if [[ "$local_line" != *"$expected_local_url"* ]]; then
  echo "server log missing expected local URL $expected_local_url" >&2
  cat "$server_log" >&2
  exit 1
fi

network_line="$(grep -E "network:[[:space:]]+" "$server_log" | tail -n 1 || true)"
if is_unspecified_host "$host"; then
  if [[ "$network_line" != *":$port" ]]; then
    echo "server log missing expected network URL for port $port" >&2
    cat "$server_log" >&2
    exit 1
  fi
else
  require_file_matches "network:[[:space:]]+[(]not detected[)]" "$server_log" \
    "server log should not advertise a separate network URL for explicit bind host $host"
fi

http_status="$(
  curl --silent --show-error \
    --output "$tmp_dir/info-response" \
    --write-out "%{http_code}" \
    "$expected_local_url$route_path"
)"
if [[ "$http_status" != "401" ]]; then
  cat "$tmp_dir/info-response" >&2
  echo "expected unauthenticated LFS info request to return HTTP 401, got $http_status" >&2
  exit 1
fi

require_file_contains '"message":"LFS Cloud authentication required"' "$tmp_dir/info-response" \
  "info route response missing expected authentication message"

cat <<CHECKLIST
LAN smoke server preflight passed.

Manual cross-machine checklist:

1. On the server machine, choose a reachable LAN base URL with no trailing slash:
   LFS_CLOUD_LAN_PUBLIC_URL=http://<server-lan-ip>:$port \\
     LFS_CLOUD_LAN_PORT=$port \\
     scripts/manual/verify-lan-smoke-test.sh
   For a real disposable-repo transfer run, point LFS_CLOUD_LAN_CONFIG at the
   real server config, make sure its server.public_url uses that same LAN URL
   with server.allow_insecure_http: true,
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
   lfs-cloud init --server http://<server-lan-ip>:$port --allow-insecure-http
   lfs-cloud login --server http://<server-lan-ip>:$port --allow-insecure-http
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
