#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$project_dir/scripts/lib/lfscloud-command.sh"
# shellcheck source=../lib/python.sh
source "$project_dir/scripts/lib/python.sh"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

python_bin="$(lfscloud_find_python3 || true)"

if [[ -z "$python_bin" ]]; then
  echo "Python 3 is required to run the pull verifier" >&2
  exit 1
fi

repo_dir="$tmp_dir/repo"
cache_root="$tmp_dir/cache"
payload_file="$tmp_dir/payload.bin"
oid_file="$tmp_dir/oid"
remote_dir="$tmp_dir/lfs-remote.git"

mkdir -p "$repo_dir"
git -C "$tmp_dir" init --quiet --bare "$(basename "$remote_dir")"

(
  cd "$repo_dir"
  git init >/dev/null
  git config user.name "LFS Cloud Pull Verifier"
  git config user.email "lfscloud-pull@example.invalid"
  git config commit.gpgSign false
  git remote add origin git@github.com:owner/repo.git
  git config remote.origin.lfsurl "$remote_dir"
  git config remote.origin.lfspushurl "$remote_dir"
)

"$python_bin" - "$repo_dir" "$payload_file" "$oid_file" <<'PY'
import hashlib
import pathlib
import sys

repo_dir = pathlib.Path(sys.argv[1])
payload_file = pathlib.Path(sys.argv[2])
oid_file = pathlib.Path(sys.argv[3])
payload = b"lfscloud pull manual verifier\n"
oid = hashlib.sha256(payload).hexdigest()
size = len(payload)
pointer = (
    "version https://git-lfs.github.com/spec/v1\n"
    f"oid sha256:{oid}\n"
    f"size {size}\n"
)
worktree_file = repo_dir / "asset" / "model.bin"
untracked_file = repo_dir / "asset" / "untracked.bin"
ordinary_pointer_file = repo_dir / "docs" / "pointer-example.txt"
source_object = repo_dir / ".git" / "lfs" / "objects" / oid[:2] / oid[2:4] / oid

worktree_file.parent.mkdir(parents=True, exist_ok=True)
ordinary_pointer_file.parent.mkdir(parents=True, exist_ok=True)
source_object.parent.mkdir(parents=True, exist_ok=True)
(repo_dir / ".gitattributes").write_text("asset/*.bin filter=lfs\n", encoding="utf-8")
worktree_file.write_text(pointer, encoding="utf-8")
untracked_file.write_text(pointer, encoding="utf-8")
ordinary_pointer_file.write_text(pointer, encoding="utf-8")
source_object.write_bytes(payload)
payload_file.write_bytes(payload)
oid_file.write_text(oid, encoding="utf-8")
PY

(
  cd "$repo_dir"
  git add .gitattributes asset/model.bin docs/pointer-example.txt
  git commit --quiet -m "Add Git LFS pull fixture"
  git lfs push origin HEAD
  rm -rf .git/lfs/objects
  run_lfscloud "$project_dir" pull --cache-root "$cache_root"
) >"$tmp_dir/pull-output"

oid="$(cat "$oid_file")"
cmp "$payload_file" "$repo_dir/asset/model.bin" >/dev/null
cmp "$payload_file" "$cache_root/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null
cmp "$payload_file" "$repo_dir/.git/lfs/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null
cmp "$payload_file" "$remote_dir/lfs/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null
grep -F "lfscloud pull" "$tmp_dir/pull-output" >/dev/null
grep -F "tracked paths: 1" "$tmp_dir/pull-output" >/dev/null
grep -F "pointers: 1" "$tmp_dir/pull-output" >/dev/null
grep -F "asset/model.bin" "$tmp_dir/pull-output" >/dev/null
if pointer_match_output="$(grep -F "oid sha256:$oid" "$repo_dir/asset/model.bin" 2>&1)"; then
  echo "pull left a pointer instead of hydrated bytes" >&2
  exit 1
else
  grep_status=$?
  if [[ "$grep_status" -ne 1 ]]; then
    echo "grep failed while checking hydrated bytes: $pointer_match_output" >&2
    exit "$grep_status"
  fi
fi
grep -F "oid sha256:$oid" "$repo_dir/asset/untracked.bin" >/dev/null
grep -F "oid sha256:$oid" "$repo_dir/docs/pointer-example.txt" >/dev/null

echo "lfscloud pull verified against fetched Git LFS cache objects"
