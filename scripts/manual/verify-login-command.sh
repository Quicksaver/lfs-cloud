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
store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
port_file="$tmp_dir/server.port"
token="manual-lfs-token"
pat="github_pat_manual"
python_bin="$(lfscloud_find_python3 || true)"

if [[ -z "$python_bin" ]]; then
  echo "Python 3 is required to run the manual login verifier" >&2
  exit 1
fi

"$python_bin" - "$port_file" "$token" "$pat" <<'PY' &
import socket
import sys

port_file, token, pat = sys.argv[1:]
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(5)
with open(port_file, "w", encoding="utf-8") as handle:
    handle.write(str(listener.getsockname()[1]))
while True:
    connection, _ = listener.accept()
    request = connection.recv(16384)
    authorization = next(
        (
            line.split(b":", 1)[1].strip()
            for line in request.split(b"\r\n")
            if line.lower().startswith(b"authorization:")
        ),
        b"",
    )
    expected_auth = ("Bearer %s" % pat).encode()
    if b"POST /auth/github/pat " in request and authorization == expected_auth:
        body = ('{"lfs_token":"%s"}' % token).encode()
        response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
    else:
        response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    connection.sendall(response)
    connection.close()
PY
server_pid="$!"

for _ in $(seq 1 50); do
  [[ -s "$port_file" ]] && break
  sleep 0.1
done
[[ -s "$port_file" ]] || { echo "test login server did not report a port" >&2; exit 1; }
port="$(<"$port_file")"
server_url="http://127.0.0.1:$port"
lfs_url="$server_url/github.com/owner/repo.git/info/lfs"
other_lfs_url="$server_url/github.com/owner/other.git/info/lfs"

mkdir -p "$repo_dir"
git -C "$repo_dir" init >/dev/null
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git
git -C "$repo_dir" config --local \
  "credential.$server_url/.useHttpPath" false

GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.helper "store --file=$store_file"

(
  cd "$repo_dir"
  printf '%s\n' "$pat" |
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
      run_lfscloud "$project_dir" login --server "$server_url"
) >"$tmp_dir/login-output"

grep -F "GitHub personal access token:" "$tmp_dir/login-output" >/dev/null
grep -F "stored local LFS credential" "$tmp_dir/login-output" >/dev/null
if grep -F "$token" "$tmp_dir/login-output" >/dev/null || grep -F "$pat" "$tmp_dir/login-output" >/dev/null; then
  echo "login output leaked a token" >&2
  exit 1
fi
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git -C "$repo_dir" config --local --get \
    "credential.$server_url/.useHttpPath" |
  grep -Fx 'true' >/dev/null

approved="$(
  printf 'url=%s\n\n' "$lfs_url" |
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
      git -C "$repo_dir" credential fill
)"

printf '%s\n' "$approved" | grep -Fx 'protocol=http' >/dev/null
printf '%s\n' "$approved" | grep -Fx "host=127.0.0.1:$port" >/dev/null
printf '%s\n' "$approved" | grep -Fx 'path=github.com/owner/repo.git/info/lfs' >/dev/null
printf '%s\n' "$approved" | grep -Fx 'username=lfscloud' >/dev/null
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

echo "lfscloud login stored a path-scoped local LFS token"
