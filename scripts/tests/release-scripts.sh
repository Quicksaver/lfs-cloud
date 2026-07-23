#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=../local/verify-all.sh
source "$SCRIPT_DIR/../local/verify-all.sh"

fail_test() {
  release_die "$1"
}

assert_eq() {
  local expected="$1"
  local actual="$2"
  local message="$3"

  if [[ "$expected" != "$actual" ]]; then
    release_warn "Expected: $expected"
    release_warn "Actual:   $actual"
    fail_test "$message"
  fi
}

release_ui_initialize "[release-tests]" "Test local release automation"
fixture_root=""
finalize_release_tests() {
  if [[ -n "$fixture_root" ]]; then
    rm -rf -- "$fixture_root"
  fi
  release_ui_finalize
}
trap finalize_release_tests EXIT
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-release-tests.XXXXXX")"

release_info "Test semantic-version increments"
assert_eq "1.0.0" "$(release_next_version "0.9.4" major)" "major increment"
assert_eq "0.10.0" "$(release_next_version "0.9.4" minor)" "minor increment"
assert_eq "0.9.5" "$(release_next_version "0.9.4" patch)" "patch increment"

release_info "Test terminal UI command status propagation"
set +e
(release_run_step "Expected command failure" bash -c 'exit 7') >/dev/null 2>&1
step_exit=$?
set -e
assert_eq "7" "$step_exit" "terminal UI command exit status"

release_info "Test bounded rolling slot output"
(
  LIVE_REGION_ENABLED=true
  ui_enable_rolling_slots 1 2
  ui_set_slot 0 running "fixture"
  ui_append_rolling_slot_output 0 "first"
  ui_append_rolling_slot_output 0 "second"
  ui_append_rolling_slot_output 0 "third"
  assert_eq "2" "${LIVE_SLOT_OUTPUT_COUNTS[0]}" "rolling output line limit"
  assert_eq "second" "${LIVE_SLOT_OUTPUT_LINES[0]}" "rolling output oldest retained line"
  assert_eq "third" "${LIVE_SLOT_OUTPUT_LINES[1]}" "rolling output newest retained line"
  ui_clear_live_state
) >/dev/null

release_info "Test Linux verifier finalizer after function scope ends"
(
  # shellcheck source=../lib/verify-linux-docker.sh
  source "$REPO_ROOT/scripts/lib/verify-linux-docker.sh"
  RELEASE_UI_INITIALIZED=0
  VERIFY_LINUX_STATUS_STARTED=1
  VERIFY_LINUX_STATUS_FINALIZED=1
  trap verify_linux_finalize_status EXIT
) >/dev/null 2>&1

release_info "Test parallel verifier orchestration and aggregate failures"
parallel_fixture="$fixture_root/parallel"
parallel_markers="$parallel_fixture/markers"
mkdir -p "$parallel_markers"
cat > "$parallel_fixture/verifier-template.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

name="$(basename "$0" .sh)"
touch "$(dirname "$0")/markers/$name"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  marker_count="$(
    find "$(dirname "$0")/markers" -type f \
      | wc -l \
      | tr -d '[:space:]'
  )"
  if [[ "$marker_count" == "3" ]]; then
    break
  fi
  sleep 0.1
done
if [[ "$marker_count" != "3" ]]; then
  printf '%s did not overlap the other verifiers\n' "$name"
  exit 9
fi

printf '%s unique output\n' "$name"
if [[ "$name" == "verifier-fail" ]]; then
  exit 7
fi
EOF
cp "$parallel_fixture/verifier-template.sh" "$parallel_fixture/verifier-one.sh"
cp "$parallel_fixture/verifier-template.sh" "$parallel_fixture/verifier-two.sh"
cp "$parallel_fixture/verifier-template.sh" "$parallel_fixture/verifier-fail.sh"
chmod +x "$parallel_fixture"/verifier-*.sh

VERIFY_ALL_LABELS=("one" "two" "expected failure")
VERIFY_ALL_COMMANDS=(
  "$parallel_fixture/verifier-one.sh"
  "$parallel_fixture/verifier-two.sh"
  "$parallel_fixture/verifier-fail.sh"
)
set +e
parallel_output="$(verify_all_run_parallel 2>&1)"
parallel_exit=$?
set -e
assert_eq "1" "$parallel_exit" "one failed verifier should produce one aggregate failure"
for verifier_name in verifier-one verifier-two verifier-fail; do
  if ! grep -q "$verifier_name unique output" <<< "$parallel_output"; then
    fail_test "parallel output omitted $verifier_name"
  fi
done

release_info "Test synchronized version metadata updates"
version_fixture="$fixture_root/version"
mkdir -p "$version_fixture"
cat > "$version_fixture/Cargo.toml" <<'EOF'
[package]
name = "lfscloud"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"

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
RELEASE_REPO_ROOT="$version_fixture"
assert_eq "1.88" "$(release_read_rust_version)" "project Rust version lookup"
RELEASE_REPO_ROOT=""

if node "$REPO_ROOT/scripts/lib/update-version.mjs" "$version_fixture" "0.1.0" "0.3.0" \
  >/dev/null 2>&1; then
  fail_test "mismatched old version should be rejected"
fi

release_info "Test commit-bound build manifest verification"
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

linux_artifact="$fixture_root/lfscloud-v0.2.0-linux-arm64-musl.tar.gz"
linux_manifest="$fixture_root/lfscloud-v0.2.0-linux-arm64-musl.build.json"
printf 'verified Linux artifact\n' > "$linux_artifact"
linux_digest="$(shasum -a 256 "$linux_artifact" | awk 'NR == 1 { print $1 }')"
jq -n \
  --arg artifact "$(basename "$linux_artifact")" \
  --arg digest "$linux_digest" \
  '{
    schema_version: 1,
    artifact: $artifact,
    commit: "abc123",
    version: "0.2.0",
    target: "aarch64-unknown-linux-musl",
    container_arch: "aarch64",
    kernel: "Linux fixture",
    rustc: "rustc fixture",
    sha256: $digest
  }' > "$linux_manifest"
release_verify_linux_manifest \
  "$linux_artifact" \
  "$linux_manifest" \
  "0.2.0" \
  "abc123" \
  "aarch64-unknown-linux-musl" \
  "aarch64"
if (
  release_verify_linux_manifest \
    "$linux_artifact" \
    "$linux_manifest" \
    "0.2.0" \
    "abc123" \
    "x86_64-unknown-linux-musl" \
    "x86_64"
) >/dev/null 2>&1; then
  fail_test "a Linux manifest for a different target should be rejected"
fi

release_info "Test exact pushed-commit and status guards"
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
  },
  {
    "id": 3,
    "context": "$LOCAL_LINUX_X86_64_STATUS_CONTEXT",
    "state": "success",
    "description": "Local Docker Linux x86-64 checks passed",
    "creator": {"login": "test-user"}
  },
  {
    "id": 4,
    "context": "$LOCAL_LINUX_ARM64_STATUS_CONTEXT",
    "state": "success",
    "description": "Local Docker Linux ARM64 checks passed",
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

release_info "Test script syntax and non-destructive help entrypoints"
bash -n \
  "$REPO_ROOT/scripts/docker/run-linux-verification.sh" \
  "$REPO_ROOT/scripts/lib/release-common.sh" \
  "$REPO_ROOT/scripts/lib/terminal-ui.sh" \
  "$REPO_ROOT/scripts/lib/verify-linux-docker.sh" \
  "$REPO_ROOT/scripts/local/verify-all.sh" \
  "$REPO_ROOT/scripts/local/verify-linux-arm64.sh" \
  "$REPO_ROOT/scripts/local/verify-linux-x86-64.sh" \
  "$REPO_ROOT/scripts/local/verify-macos.sh" \
  "$REPO_ROOT/scripts/release.sh" \
  "$REPO_ROOT/scripts/tests/release-scripts.sh"
"$REPO_ROOT/scripts/local/verify-linux-arm64.sh" --help >/dev/null
"$REPO_ROOT/scripts/local/verify-linux-x86-64.sh" --help >/dev/null
"$REPO_ROOT/scripts/local/verify-macos.sh" --help >/dev/null
"$REPO_ROOT/scripts/local/verify-all.sh" --help >/dev/null
"$REPO_ROOT/scripts/release.sh" --help >/dev/null

release_pass "Release script tests"
