#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=../release-all.sh
source "$SCRIPT_DIR/../release-all.sh"

TESTS_PASSED=0

assert_equal() {
  local expected="$1"
  local actual="$2"
  local message="$3"

  if [[ "$actual" != "$expected" ]]; then
    printf '[release-all-tests] FAIL: %s (expected %q, got %q)\n' \
      "$message" "$expected" "$actual" >&2
    exit 1
  fi
  TESTS_PASSED=$((TESTS_PASSED + 1))
}

assert_contains() {
  local text="$1"
  local expected="$2"
  local message="$3"

  if [[ "$text" != *"$expected"* ]]; then
    printf '[release-all-tests] FAIL: %s (missing %q)\n' \
      "$message" "$expected" >&2
    exit 1
  fi
  TESTS_PASSED=$((TESTS_PASSED + 1))
}

fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-release-all-tests.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT

assert_equal \
  'E:\Projects\lfs-cloud' \
  "$RELEASE_ALL_WINDOWS_REPO" \
  'fleet Windows checkout default'
assert_equal \
  'windows-desktop' \
  "$RELEASE_ALL_WINDOWS_HOST" \
  'fleet Windows SSH alias default'

if release_all_validate_windows_repo_path 'C:\Projects\lfs-cloud' >/dev/null 2>&1; then
  printf '[release-all-tests] FAIL: Windows checkout outside E: was accepted\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))
release_all_validate_windows_repo_path 'E:\Projects\lfs-cloud'
TESTS_PASSED=$((TESTS_PASSED + 1))

sync_script="$(release_all_windows_sync_script '0123456789abcdef0123456789abcdef01234567')"
assert_contains "$sync_script" "\$repo = 'E:\Projects\lfs-cloud'" 'sync uses fleet checkout'
assert_contains "$sync_script" 'Set-Location -LiteralPath $repo' 'sync enters fleet checkout'
assert_contains "$sync_script" "'main'" 'sync requires main'
assert_contains "$sync_script" 'merge --ff-only' 'sync only fast-forwards Windows'
assert_contains \
  "$sync_script" \
  '0123456789abcdef0123456789abcdef01234567' \
  'sync requires the expected commit'

release_script="$(release_all_windows_release_script 'v1.2.3')"
assert_contains "$release_script" "-Tag 'v1.2.3'" 'Windows continuation receives the exact tag'

RELEASE_REPO_ROOT="$fixture_root"
RELEASE_ALL_SHA='0123456789abcdef0123456789abcdef01234567'
if release_all_local_artifacts_are_valid macos-arm64 1.2.3 >/dev/null 2>&1; then
  printf '[release-all-tests] FAIL: missing local artifacts were treated as reusable\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

mkdir -p "$fixture_root/logs/release-old" "$fixture_root/logs/release-recent"
printf old >"$fixture_root/logs/release-old/old.log"
printf recent >"$fixture_root/logs/release-recent/recent.log"
touch -t 202001010000 "$fixture_root/logs/release-old/old.log"
release_all_prune_logs
if [[ -e "$fixture_root/logs/release-old/old.log" ]] \
  || [[ ! -e "$fixture_root/logs/release-recent/recent.log" ]]; then
  printf '[release-all-tests] FAIL: coordinator log retention was not bounded correctly\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

event_file="$fixture_root/events"
real_ensure_base_verifications="$(declare -f release_all_ensure_base_verifications)"
release_all_preflight() { printf 'preflight\n' >>"$event_file"; }
release_all_ensure_base_verifications() { printf 'base-verifiers\n' >>"$event_file"; }
release_all_prepare_candidate() {
  printf 'prepare:%s\n' "$1" >>"$event_file"
  RELEASE_ALL_TAG='v1.2.3'
  RELEASE_ALL_SHA='0123456789abcdef0123456789abcdef01234567'
  RELEASE_ALL_IS_DRAFT=true
}
release_all_sync_windows() { printf 'sync:%s\n' "$1" >>"$event_file"; }
release_all_verify_candidate() { printf 'release-verifiers:%s\n' "$1" >>"$event_file"; }
release_all_complete_local_draft() { printf 'complete:%s\n' "$1" >>"$event_file"; }
release_all_require_all_green() { printf 'green:%s\n' "$1" >>"$event_file"; }
release_all_publish_candidate() { printf 'publish:%s\n' "$1" >>"$event_file"; }
release_all_initialize_ui() { :; }
release_all_finalize_ui() { :; }

release_all_main patch
assert_equal "$(cat <<'EOF'
preflight
base-verifiers
prepare:patch
sync:0123456789abcdef0123456789abcdef01234567
release-verifiers:v1.2.3
complete:v1.2.3
green:0123456789abcdef0123456789abcdef01234567
publish:v1.2.3
EOF
)" "$(<"$event_file")" 'all-in-one release phase order'

eval "$real_ensure_base_verifications"
RELEASE_ALL_RESUMING=true
release_all_collect_missing_base_checks() {
  printf 'unexpected-base-collection\n' >>"$event_file"
}
: >"$event_file"
release_all_ensure_base_verifications
assert_equal '' "$(<"$event_file")" 'resumed release skips ordinary base verification'
RELEASE_ALL_RESUMING=false

: >"$event_file"
release_all_ensure_base_verifications() {
  printf 'base-verifiers\n' >>"$event_file"
  return 9
}
set +e
release_all_main minor >/dev/null 2>&1
failure_exit=$?
set -e
assert_equal '9' "$failure_exit" 'base verification failure status'
assert_equal "$(cat <<'EOF'
preflight
base-verifiers
EOF
)" "$(<"$event_file")" 'failure stops later release phases'

release_all_initialize_ui() { :; }
release_all_finalize_ui() { :; }
RELEASE_REPO_ROOT="$fixture_root"
RELEASE_ALL_TAG='v1.2.3'
termination_marker="$fixture_root/terminated"
release_all_run_local_verifiers() {
  trap 'printf terminated >"$termination_marker"; exit 143' TERM
  while true; do sleep 0.1; done
}
release_all_run_windows_action() { return 7; }
set +e
release_all_run_verification_wave fail-fast release macos-arm64 >/dev/null 2>&1
wave_exit=$?
set -e
assert_equal '7' "$wave_exit" 'parallel verification returns the first failure'
if [[ ! -e "$termination_marker" ]]; then
  printf '[release-all-tests] FAIL: sibling verifier was not terminated\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

printf '[release-all-tests] %d passed, 0 failed\n' "$TESTS_PASSED"
