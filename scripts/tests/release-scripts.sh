#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=../lib/release-common.sh
source "$SCRIPT_DIR/../lib/release-common.sh"

fail_test() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

assert_eq() {
  local expected="$1"
  local actual="$2"
  local message="$3"

  if [[ "$expected" != "$actual" ]]; then
    printf 'Expected: %s\nActual:   %s\n' "$expected" "$actual" >&2
    fail_test "$message"
  fi
}

fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-release-tests.XXXXXX")"
trap 'rm -rf -- "$fixture_root"' EXIT

printf '%s\n' "Test semantic-version increments"
assert_eq "1.0.0" "$(release_next_version "0.9.4" major)" "major increment"
assert_eq "0.10.0" "$(release_next_version "0.9.4" minor)" "minor increment"
assert_eq "0.9.5" "$(release_next_version "0.9.4" patch)" "patch increment"

printf '%s\n' "Test synchronized version metadata updates"
version_fixture="$fixture_root/version"
mkdir -p "$version_fixture"
cat > "$version_fixture/Cargo.toml" <<'EOF'
[package]
name = "lfscloud"
version = "0.1.0"
edition = "2024"

[dependencies]
version = "1"
EOF
cat > "$version_fixture/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "dependency"
version = "9.8.7"

[[package]]
name = "lfscloud"
version = "0.1.0"
dependencies = [
 "dependency",
]
EOF
cat > "$version_fixture/package.json" <<'EOF'
{
  "name": "lfscloud",
  "version": "0.1.0",
  "private": true
}
EOF
chmod 644 "$version_fixture/Cargo.toml" "$version_fixture/Cargo.lock" "$version_fixture/package.json"

node "$REPO_ROOT/scripts/lib/update-version.mjs" "$version_fixture" "0.1.0" "0.2.0"
assert_eq "0.2.0" "$(
  awk '/^version = "/ { value = $0; sub(/^version = "/, "", value); sub(/"$/, "", value); print value; exit }' \
    "$version_fixture/Cargo.toml"
)" "Cargo.toml version update"
assert_eq "0.2.0" "$(
  awk '
    /^name = "lfscloud"$/ { found = 1; next }
    found && /^version = "/ {
      value = $0
      sub(/^version = "/, "", value)
      sub(/"$/, "", value)
      print value
      exit
    }
  ' "$version_fixture/Cargo.lock"
)" "Cargo.lock version update"
assert_eq "0.2.0" "$(
  node -e 'process.stdout.write(require(process.argv[1]).version)' "$version_fixture/package.json"
)" "package.json version update"
assert_eq "644" "$(stat -f '%Lp' "$version_fixture/Cargo.toml")" "metadata file mode preservation"

if node "$REPO_ROOT/scripts/lib/update-version.mjs" "$version_fixture" "0.1.0" "0.3.0" \
  >/dev/null 2>&1; then
  fail_test "mismatched old version should be rejected"
fi

printf '%s\n' "Test commit-bound build manifest verification"
manifest_artifact="$fixture_root/lfscloud-v0.2.0-macos-arm64.tar.gz"
manifest_file="$fixture_root/lfscloud-v0.2.0-macos-arm64.build.json"
printf 'verified artifact\n' > "$manifest_artifact"
manifest_digest="$(shasum -a 256 "$manifest_artifact" | awk 'NR == 1 { print $1 }')"
jq -n \
  --arg artifact "$(basename "$manifest_artifact")" \
  --arg digest "$manifest_digest" \
  '{
    schema_version: 1,
    artifact: $artifact,
    commit: "abc123",
    version: "0.2.0",
    target: "aarch64-apple-darwin",
    macos: "26.5.2",
    rustc: "rustc system",
    sha256: $digest
  }' > "$manifest_file"
release_verify_macos_manifest "$manifest_artifact" "$manifest_file" "0.2.0" "abc123"
if (
  release_verify_macos_manifest "$manifest_artifact" "$manifest_file" "0.2.0" "different"
) >/dev/null 2>&1; then
  fail_test "a build manifest for a different commit should be rejected"
fi

printf '%s\n' "Test exact pushed-commit and status guards"
bare_repo="$fixture_root/origin.git"
work_repo="$fixture_root/work"
fake_bin="$fixture_root/bin"
status_file="$fixture_root/statuses.json"
mkdir -p "$fake_bin"

git init --bare --quiet "$bare_repo"
git init --quiet "$work_repo"
git -C "$work_repo" config user.name "Release Script Test"
git -C "$work_repo" config user.email "release-script@example.invalid"
git -C "$work_repo" checkout -q -b main
printf 'fixture\n' > "$work_repo/fixture.txt"
git -C "$work_repo" add fixture.txt
git -C "$work_repo" commit --quiet -m "Initial fixture"
git -C "$work_repo" remote add origin https://github.com/example/project.git
git -C "$work_repo" config "url.$bare_repo.insteadOf" https://github.com/example/project.git
git -C "$work_repo" push --quiet -u origin main

cat > "$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
  exit 0
fi

if [[ "$1" != "api" ]]; then
  exit 2
fi
shift

if [[ "${1:-}" == "--method" ]]; then
  printf '%s\n' "$*" >> "$FAKE_GH_POSTS"
  exit 0
fi

endpoint="$1"
if [[ "$endpoint" == "user" ]]; then
  printf '%s\n' "test-user"
elif [[ "$endpoint" == *"/statuses?per_page=100" ]]; then
  cat "$FAKE_GH_STATUSES"
elif [[ "$endpoint" == *"/commits/"* ]]; then
  printf '%s\n' "${endpoint##*/}"
else
  exit 2
fi
EOF
chmod +x "$fake_bin/gh"

cat > "$status_file" <<EOF
[
  {
    "id": 1,
    "context": "$LOCAL_MACOS_STATUS_CONTEXT",
    "state": "failure",
    "description": "old result",
    "creator": {"login": "test-user"}
  },
  {
    "id": 2,
    "context": "$LOCAL_MACOS_STATUS_CONTEXT",
    "state": "success",
    "description": "Local macOS checks passed",
    "creator": {"login": "test-user"}
  }
]
EOF

export PATH="$fake_bin:$PATH"
export FAKE_GH_STATUSES="$status_file"
export FAKE_GH_POSTS="$fixture_root/posts.txt"
: > "$FAKE_GH_POSTS"

(
  release_initialize "$work_repo"
  release_require_current_commit_on_origin
  release_require_local_statuses_green "$RELEASE_SHA"
  release_post_status "$RELEASE_SHA" "$LOCAL_MACOS_STATUS_CONTEXT" pending "Local checks running"
)
if ! grep -q "context=$LOCAL_MACOS_STATUS_CONTEXT" "$FAKE_GH_POSTS"; then
  fail_test "status post did not preserve the local context"
fi

printf 'ahead\n' >> "$work_repo/fixture.txt"
git -C "$work_repo" add fixture.txt
git -C "$work_repo" commit --quiet -m "Ahead of origin"
if (
  release_initialize "$work_repo"
  release_require_current_commit_on_origin
) >/dev/null 2>&1; then
  fail_test "an unpushed commit should be rejected"
fi

git -C "$work_repo" reset --hard --quiet origin/main
cat > "$status_file" <<EOF
[
  {
    "id": 3,
    "context": "$LOCAL_MACOS_STATUS_CONTEXT",
    "state": "success",
    "description": "wrong creator",
    "creator": {"login": "github-actions[bot]"}
  }
]
EOF
if (
  release_initialize "$work_repo"
  release_require_local_statuses_green "$RELEASE_SHA"
) >/dev/null 2>&1; then
  fail_test "a status from a different creator should be rejected"
fi

printf '%s\n' "Test script syntax and non-destructive help entrypoints"
bash -n \
  "$REPO_ROOT/scripts/lib/release-common.sh" \
  "$REPO_ROOT/scripts/local/verify-macos.sh" \
  "$REPO_ROOT/scripts/release.sh" \
  "$REPO_ROOT/scripts/tests/release-scripts.sh"
"$REPO_ROOT/scripts/local/verify-macos.sh" --help >/dev/null
"$REPO_ROOT/scripts/release.sh" --help >/dev/null

printf 'PASS: release script tests\n'
