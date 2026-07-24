#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$project_dir/scripts/lib/lfscloud-command.sh"
# shellcheck source=../lib/python.sh
source "$project_dir/scripts/lib/python.sh"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to run the manual local-cache CLI verifier" >&2
  exit 1
fi

if ! command -v git >/dev/null 2>&1; then
  echo "git is required to run the manual local-cache CLI verifier" >&2
  exit 1
fi

if ! git lfs version >/dev/null 2>&1; then
  echo "git lfs is required to run the manual local-cache CLI verifier" >&2
  exit 1
fi

# Python 3 is used only to create binary fixtures and compute the expected OID.
python_bin="$(lfscloud_find_python3 || true)"

if [[ -z "$python_bin" ]]; then
  echo "Python 3 is required to run the manual local-cache CLI verifier" >&2
  exit 1
fi

repo_dir="$tmp_dir/repo"
cache_root="$tmp_dir/cache"
payload_file="$tmp_dir/payload.bin"
oid_file="$tmp_dir/oid"
pointer_file="$tmp_dir/pointer"

"$python_bin" - "$cache_root" "$payload_file" "$oid_file" <<'PY'
import hashlib
import pathlib
import sys

cache_root = pathlib.Path(sys.argv[1])
payload_file = pathlib.Path(sys.argv[2])
oid_file = pathlib.Path(sys.argv[3])
payload = b"lfscloud hydrate/dehydrate CLI manual verifier\n"
oid = hashlib.sha256(payload).hexdigest()
cache_path = cache_root / "objects" / oid[:2] / oid[2:4] / oid

cache_path.parent.mkdir(parents=True, exist_ok=True)
cache_path.write_bytes(payload)
payload_file.write_bytes(payload)
oid_file.write_text(oid, encoding="utf-8")
PY

git -C "$tmp_dir" init --quiet repo
git -C "$repo_dir" config user.name "LFS Cloud Manual Verifier"
git -C "$repo_dir" config user.email "lfscloud-manual@example.invalid"
git -C "$repo_dir" config commit.gpgSign false
git -C "$repo_dir" remote add origin git@github.com:lfscloud/manual-verifier.git
git -C "$repo_dir" lfs install --local >/dev/null
git -C "$repo_dir" lfs track "asset/model.bin" >/dev/null
mkdir -p "$repo_dir/asset"
cp "$payload_file" "$repo_dir/asset/model.bin"
git -C "$repo_dir" add .gitattributes asset/model.bin
git -C "$repo_dir" commit --quiet -m "Add Git LFS fixture"
git -C "$repo_dir" show HEAD:asset/model.bin >"$pointer_file"
cp "$pointer_file" "$repo_dir/asset/model.bin"
rm -rf "$repo_dir/.git/lfs/objects"

(
  cd "$repo_dir"
  run_lfscloud "$project_dir" hydrate --cache-root "$cache_root" asset/model.bin
) >"$tmp_dir/hydrate-output"

cmp "$payload_file" "$repo_dir/asset/model.bin" >/dev/null

(
  cd "$repo_dir"
  run_lfscloud "$project_dir" dehydrate --cache-root "$cache_root" asset/model.bin
) >"$tmp_dir/dehydrate-output"

oid="$(cat "$oid_file")"
cmp "$payload_file" "$cache_root/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null
cmp "$pointer_file" "$repo_dir/asset/model.bin" >/dev/null
cmp "$payload_file" "$repo_dir/.git/lfs/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null

git -C "$tmp_dir" init --quiet --bare lfs-remote.git
git -C "$repo_dir" remote add lfs-verifier "$tmp_dir/lfs-remote.git"
git -C "$repo_dir" lfs push lfs-verifier HEAD
cmp "$payload_file" "$tmp_dir/lfs-remote.git/lfs/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null

grep -F "hydrated" "$tmp_dir/hydrate-output" >/dev/null
tr '\\' '/' <"$tmp_dir/hydrate-output" | grep -F "asset/model.bin" >/dev/null
grep -F "dehydrated" "$tmp_dir/dehydrate-output" >/dev/null
tr '\\' '/' <"$tmp_dir/dehydrate-output" | grep -F "asset/model.bin" >/dev/null

echo "lfscloud hydrate/dehydrate and Git LFS push verified"
