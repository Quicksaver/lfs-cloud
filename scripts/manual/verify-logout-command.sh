#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$project_dir/scripts/lib/lfscloud-command.sh"
tmp_dir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

repo_dir="$tmp_dir/repo"
store_file="$tmp_dir/credentials"
global_config="$tmp_dir/gitconfig"
port_file="$tmp_dir/port"
request_file="$tmp_dir/request"
token="manual-logout-lfs-token"
python_bin="$(command -v python3 || command -v python || true)"

if [[ -z "$python_bin" ]]; then
  echo "Python 3 is required to run the logout verifier" >&2
  exit 1
fi

mkdir -p "$repo_dir"
git -C "$repo_dir" init --quiet
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git

"$python_bin" - "$port_file" "$request_file" "$token" <<'PY' &
import http.server
import pathlib
import sys

port_file = pathlib.Path(sys.argv[1])
request_file = pathlib.Path(sys.argv[2])
expected_token = sys.argv[3]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_DELETE(self):
        if self.path != "/auth/session":
            self.send_response(404)
            self.end_headers()
            return
        if self.headers.get("Authorization") != f"Bearer {expected_token}":
            self.send_response(401)
            self.end_headers()
            return
        request_file.write_text("authenticated session revocation\n")
        self.send_response(204)
        self.end_headers()

    def log_message(self, _format, *_args):
        pass

server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
port_file.write_text(str(server.server_port))
server.handle_request()
PY
server_pid="$!"

for _ in {1..100}; do
  [[ -s "$port_file" ]] && break
  sleep 0.05
done
[[ -s "$port_file" ]]

port="$(cat "$port_file")"
server_url="http://127.0.0.1:$port"
lfs_url="$server_url/github.com/owner/repo.git/info/lfs"

GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
  git config --global credential.helper "store --file=$store_file"
git -C "$repo_dir" config --local \
  "credential.$server_url/.useHttpPath" true
printf 'url=%s\nusername=lfscloud\npassword=%s\n\n' "$lfs_url" "$token" |
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    git -C "$repo_dir" credential approve

(
  cd "$repo_dir"
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    run_lfscloud "$project_dir" logout --server "$server_url"
) >"$tmp_dir/logout-output"

wait "$server_pid"
server_pid=""
grep -Fx 'authenticated session revocation' "$request_file" >/dev/null
grep -F 'revoked local LFS session' "$tmp_dir/logout-output" >/dev/null
grep -F 'erased local LFS credential' "$tmp_dir/logout-output" >/dev/null
if grep -F "$token" "$tmp_dir/logout-output" >/dev/null; then
  echo "logout output leaked the local LFS token" >&2
  exit 1
fi
if printf 'url=%s\nusername=lfscloud\n\n' "$lfs_url" |
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$global_config" \
    git -C "$repo_dir" credential fill >/dev/null 2>&1; then
  echo "logout left the local LFS credential available" >&2
  exit 1
fi

echo "lfscloud logout revoked the session and erased its Git credential"
