#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

python_bin="$(command -v python3 || command -v python || true)"

if [[ -z "$python_bin" ]] || ! "$python_bin" - <<'PY'
import sys

sys.exit(0 if sys.version_info[0] >= 3 else 1)
PY
then
  echo "Python 3 is required to run the manual local-cache CLI verifier" >&2
  exit 1
fi

repo_dir="$tmp_dir/repo"
cache_root="$tmp_dir/cache"
payload_file="$tmp_dir/payload.bin"
oid_file="$tmp_dir/oid"

"$python_bin" - "$repo_dir" "$cache_root" "$payload_file" "$oid_file" <<'PY'
import hashlib
import pathlib
import sys

repo_dir = pathlib.Path(sys.argv[1])
cache_root = pathlib.Path(sys.argv[2])
payload_file = pathlib.Path(sys.argv[3])
oid_file = pathlib.Path(sys.argv[4])
payload = b"lfs-cloud hydrate/dehydrate CLI manual verifier\n"
oid = hashlib.sha256(payload).hexdigest()
size = len(payload)
cache_path = cache_root / "objects" / oid[:2] / oid[2:4] / oid
worktree_file = repo_dir / "asset" / "model.bin"
pointer = (
    "version https://git-lfs.github.com/spec/v1\n"
    f"oid sha256:{oid}\n"
    f"size {size}\n"
)

cache_path.parent.mkdir(parents=True, exist_ok=True)
worktree_file.parent.mkdir(parents=True, exist_ok=True)
cache_path.write_bytes(payload)
payload_file.write_bytes(payload)
worktree_file.write_text(pointer, encoding="utf-8")
oid_file.write_text(oid, encoding="utf-8")
PY

(
  cd "$repo_dir"
  cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- \
    hydrate --cache-root "$cache_root" asset/model.bin
) >"$tmp_dir/hydrate-output"

cmp "$payload_file" "$repo_dir/asset/model.bin" >/dev/null

(
  cd "$repo_dir"
  cargo run --quiet --manifest-path "$project_dir/Cargo.toml" -- \
    dehydrate --cache-root "$cache_root" asset/model.bin
) >"$tmp_dir/dehydrate-output"

oid="$(cat "$oid_file")"
cmp "$payload_file" "$cache_root/objects/${oid:0:2}/${oid:2:2}/$oid" >/dev/null
grep -F "oid sha256:$oid" "$repo_dir/asset/model.bin" >/dev/null
grep -F "hydrated" "$tmp_dir/hydrate-output" >/dev/null
grep -F "dehydrated" "$tmp_dir/dehydrate-output" >/dev/null

echo "lfs-cloud hydrate/dehydrate verified against the shared local cache"
