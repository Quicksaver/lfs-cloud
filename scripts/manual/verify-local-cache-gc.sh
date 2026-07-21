#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$project_dir/scripts/lib/lfscloud-command.sh"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# Python 3 is used only to create deterministic binary fixtures and registry JSON.
python_bin="$(command -v python3 || command -v python || true)"

if [[ -z "$python_bin" ]] || ! "$python_bin" - <<'PY'
import sys

sys.exit(0 if sys.version_info[0] >= 3 else 1)
PY
then
  echo "Python 3 is required to run the manual local-cache GC verifier" >&2
  exit 1
fi

if ! command -v git >/dev/null 2>&1; then
  echo "git is required to run the manual local-cache GC verifier" >&2
  exit 1
fi

repo_dir="$tmp_dir/repo"
missing_repo_dir="$tmp_dir/missing-repo"
cache_root="$tmp_dir/cache"
keep_oid_file="$tmp_dir/keep-oid"
remove_oid_file="$tmp_dir/remove-oid"

"$python_bin" - "$repo_dir" "$missing_repo_dir" "$cache_root" "$keep_oid_file" "$remove_oid_file" <<'PY'
import hashlib
import json
import pathlib
import sys

repo_dir = pathlib.Path(sys.argv[1])
missing_repo_dir = pathlib.Path(sys.argv[2])
cache_root = pathlib.Path(sys.argv[3])
keep_oid_file = pathlib.Path(sys.argv[4])
remove_oid_file = pathlib.Path(sys.argv[5])

keep_payload = b"lfscloud gc retained payload\n"
remove_payload = b"lfscloud gc removable payload\n"

def cache_object(payload: bytes) -> str:
    oid = hashlib.sha256(payload).hexdigest()
    cache_path = cache_root / "objects" / oid[:2] / oid[2:4] / oid
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    cache_path.write_bytes(payload)
    return oid

keep_oid = cache_object(keep_payload)
remove_oid = cache_object(remove_payload)
pointer = (
    "version https://git-lfs.github.com/spec/v1\n"
    f"oid sha256:{keep_oid}\n"
    f"size {len(keep_payload)}\n"
)

(repo_dir / "asset").mkdir(parents=True, exist_ok=True)
(repo_dir / "asset" / "model.bin").write_text(pointer, encoding="utf-8")

(cache_root / "worktrees.json").write_text(
    json.dumps(
        {
            "version": 1,
            "worktrees": [
                {
                    "repository_id": "github-main:owner/repo",
                    "worktree_root": str(repo_dir),
                    "git_dir": str(repo_dir / ".git"),
                },
                {
                    "repository_id": "github-main:owner/missing",
                    "worktree_root": str(missing_repo_dir),
                    "git_dir": str(missing_repo_dir / ".git"),
                },
            ],
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)

keep_oid_file.write_text(keep_oid, encoding="utf-8")
remove_oid_file.write_text(remove_oid, encoding="utf-8")
PY

git -C "$repo_dir" init >/dev/null
git -C "$repo_dir" remote add origin git@github.com:owner/repo.git
printf '*.bin filter=lfs\n' >"$repo_dir/.gitattributes"
git -C "$repo_dir" add -- .gitattributes asset/model.bin

keep_oid="$(cat "$keep_oid_file")"
remove_oid="$(cat "$remove_oid_file")"
keep_cache_path="$cache_root/objects/${keep_oid:0:2}/${keep_oid:2:2}/$keep_oid"
remove_cache_path="$cache_root/objects/${remove_oid:0:2}/${remove_oid:2:2}/$remove_oid"

(
  cd "$repo_dir"
  run_lfscloud "$project_dir" gc --cache-root "$cache_root" --dry-run >"$tmp_dir/gc-dry-run-output"
)

test -f "$keep_cache_path"
test -f "$remove_cache_path"
grep -F "protected while worktree unavailable" "$tmp_dir/gc-dry-run-output" >/dev/null
grep -F "$remove_oid" "$tmp_dir/gc-dry-run-output" >/dev/null
grep -F "missing-repo" "$cache_root/worktrees.json" >/dev/null

(
  cd "$repo_dir"
  run_lfscloud "$project_dir" gc --cache-root "$cache_root" >"$tmp_dir/gc-output"
)

test -f "$keep_cache_path"
test -f "$remove_cache_path"
grep -F "protected while worktree unavailable" "$tmp_dir/gc-output" >/dev/null
grep -F "$remove_oid" "$tmp_dir/gc-output" >/dev/null
grep -F "missing-repo" "$cache_root/worktrees.json" >/dev/null

(
  cd "$repo_dir"
  run_lfscloud "$project_dir" gc --cache-root "$cache_root" --prune-unavailable-worktrees \
    >"$tmp_dir/gc-prune-output"
)

test -f "$keep_cache_path"
test ! -e "$remove_cache_path"
grep -F "removed" "$tmp_dir/gc-prune-output" >/dev/null
grep -F "$remove_oid" "$tmp_dir/gc-prune-output" >/dev/null
if grep -F "missing-repo" "$cache_root/worktrees.json" >/dev/null; then
  echo "explicitly pruned worktree registration remains" >&2
  exit 1
fi

echo "lfscloud gc verified unavailable-root protection and explicit pruning"
