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
restore_fixture_cleanup_trap() {
  trap 'rm -rf "$fixture_root"' EXIT
}
restore_fixture_cleanup_trap

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
release_all_validate_windows_repo_path 'E:/Projects/lfs-cloud'
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

candidate_assets_script="$(
  release_all_windows_candidate_assets_script \
    'v1.2.3' \
    '0123456789abcdef0123456789abcdef01234567'
)"
assert_contains \
  "$candidate_assets_script" \
  "Initialize-Release -StartDirectory \$repo" \
  'candidate reuse initializes release metadata'
assert_contains \
  "$candidate_assets_script" \
  "Test-ArtifactChecksum -ArtifactPath \$artifact" \
  'candidate reuse validates the Windows checksum'
assert_contains \
  "$candidate_assets_script" \
  "Test-WindowsBuildManifest -ArtifactPath \$artifact" \
  'candidate reuse validates the Windows manifest'
assert_contains \
  "$candidate_assets_script" \
  "Assert-WindowsReleaseAssetsPublished -Release \$release" \
  'candidate reuse validates the remote Windows assets'
assert_contains \
  "$candidate_assets_script" \
  '-ErrorAction Continue' \
  'candidate reuse preserves the recoverable validation exit code'

ssh_arguments_file="$fixture_root/ssh-arguments"
ssh() { printf '%s\n' "$@" >"$ssh_arguments_file"; }
release_all_windows_execute_script 'Write-Output fixture'
unset -f ssh
assert_equal '-n' "$(sed -n '1p' "$ssh_arguments_file")" 'Windows SSH detaches stdin'

RELEASE_REPO_ROOT="$fixture_root"
RELEASE_ALL_SHA='0123456789abcdef0123456789abcdef01234567'
if release_all_local_artifacts_are_valid macos-arm64 1.2.3 >/dev/null 2>&1; then
  printf '[release-all-tests] FAIL: missing local artifacts were treated as reusable\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

real_release_verify_checksum="$(declare -f release_verify_checksum)"
real_release_verify_macos_manifest="$(declare -f release_verify_macos_manifest)"
release_verify_checksum() { :; }
release_verify_macos_manifest() { :; }
if release_all_local_artifacts_are_valid macos-arm64 1.2.3 >/dev/null 2>&1; then
  printf '[release-all-tests] FAIL: macOS artifacts without the executable were reused\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))
mkdir -p "$fixture_root/target/aarch64-apple-darwin/release"
printf '#!/usr/bin/env sh\nprintf "lfscloud 1.2.3\\n"\n' \
  >"$fixture_root/target/aarch64-apple-darwin/release/lfscloud"
chmod +x "$fixture_root/target/aarch64-apple-darwin/release/lfscloud"
if ! release_all_local_artifacts_are_valid macos-arm64 1.2.3 >/dev/null 2>&1; then
  printf '[release-all-tests] FAIL: complete reusable macOS state was rejected\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))
eval "$real_release_verify_checksum"
eval "$real_release_verify_macos_manifest"

real_release_all_status_is_green="$(declare -f release_all_status_is_green)"
real_release_all_local_artifacts_are_valid="$(
  declare -f release_all_local_artifacts_are_valid
)"
real_release_all_windows_candidate_assets_are_valid="$(
  declare -f release_all_windows_candidate_assets_are_valid
)"
release_all_status_is_green() { return 0; }
release_all_local_artifacts_are_valid() { return 0; }
release_all_windows_candidate_assets_are_valid() { return 20; }
RELEASE_ALL_TAG='v1.2.3'
release_all_collect_missing_candidate_checks
assert_equal 'true' "$RELEASE_ALL_WINDOWS_NEEDED" 'missing Windows assets require continuation'
release_all_windows_candidate_assets_are_valid() { return 37; }
set +e
release_all_collect_missing_candidate_checks >/dev/null 2>&1
candidate_transport_exit=$?
set -e
assert_equal '37' "$candidate_transport_exit" 'Windows asset transport failure is surfaced'
release_all_windows_candidate_assets_are_valid() { return 0; }
release_all_collect_missing_candidate_checks
assert_equal 'false' "$RELEASE_ALL_WINDOWS_NEEDED" 'verified Windows assets permit reuse'
eval "$real_release_all_status_is_green"
eval "$real_release_all_local_artifacts_are_valid"
eval "$real_release_all_windows_candidate_assets_are_valid"

real_release_initialize="$(declare -f release_initialize)"
real_release_require_command="$(declare -f release_require_command)"
real_release_require_fully_clean="$(declare -f release_require_fully_clean)"
real_release_require_current_commit_on_origin="$(
  declare -f release_require_current_commit_on_origin
)"
real_release_all_sync_windows="$(declare -f release_all_sync_windows)"
real_release_require_matching_versions="$(declare -f release_require_matching_versions)"
preflight_marker="$fixture_root/preflight-continued"
release_initialize() {
  RELEASE_REPO_ROOT="$fixture_root"
  RELEASE_BRANCH=main
  RELEASE_SHA='0123456789abcdef0123456789abcdef01234567'
}
release_require_command() { :; }
release_require_fully_clean() { :; }
release_require_current_commit_on_origin() { :; }
release_all_sync_windows() { return 19; }
release_require_matching_versions() {
  printf continued >"$preflight_marker"
  printf '1.2.3\n'
}
set +e
release_all_preflight >/dev/null 2>&1
preflight_exit=$?
set -e
cd "$REPO_ROOT"
assert_equal '19' "$preflight_exit" 'Windows preflight sync failure status'
if [[ -e "$preflight_marker" ]]; then
  printf '[release-all-tests] FAIL: Windows sync failure did not stop preflight\n' >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))
eval "$real_release_initialize"
eval "$real_release_require_command"
eval "$real_release_require_fully_clean"
eval "$real_release_require_current_commit_on_origin"
eval "$real_release_all_sync_windows"
eval "$real_release_require_matching_versions"

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

annotated_tag_sha='0123456789abcdef0123456789abcdef01234567'
git() {
  printf '%s\trefs/tags/v1.2.3\n' '1111111111111111111111111111111111111111'
  printf '%s\trefs/tags/v1.2.3^{}\n' "$annotated_tag_sha"
}
assert_equal \
  "$annotated_tag_sha" \
  "$(release_all_remote_tag_commit v1.2.3)" \
  'remote annotated tag resolves to its peeled commit'
git() { return 23; }
set +e
remote_failure="$(release_all_remote_tag_commit v1.2.3 2>&1)"
remote_failure_exit=$?
set -e
unset -f git
assert_equal '1' "$remote_failure_exit" 'remote tag transport failure status'
assert_contains \
  "$remote_failure" \
  "Could not read release tag 'v1.2.3' from origin." \
  'remote tag transport failure message'

real_script_dir="$SCRIPT_DIR"
real_release_require_matching_versions="$(declare -f release_require_matching_versions)"
real_release_all_remote_tag_commit="$(declare -f release_all_remote_tag_commit)"
real_release_all_current_release_document="$(declare -f release_all_current_release_document)"
candidate_script_dir="$fixture_root/candidate-script"
candidate_calls="$fixture_root/candidate-calls"
mkdir -p "$candidate_script_dir"
cat >"$candidate_script_dir/release.sh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$RELEASE_ALL_TEST_CANDIDATE_CALLS"
EOF
chmod +x "$candidate_script_dir/release.sh"
export RELEASE_ALL_TEST_CANDIDATE_CALLS="$candidate_calls"
SCRIPT_DIR="$candidate_script_dir"
RELEASE_SHA='0123456789abcdef0123456789abcdef01234567'
release_require_matching_versions() { printf '1.2.3\n'; }
release_all_current_release_document() {
  printf '%s\n' '{"isDraft":true,"tagName":"v1.2.3"}'
}
candidate_local_tag=''
candidate_remote_tag=''
git() {
  case "$1:${2:-}:${3:-}" in
    'log:-1:--format=%s') printf 'Release v1.2.3\n' ;;
    'rev-list:-n:1') printf '%s\n' "$candidate_local_tag" ;;
    'rev-parse:HEAD:') printf '%s\n' "$RELEASE_SHA" ;;
    *) return 2 ;;
  esac
}
release_all_remote_tag_commit() { printf '%s\n' "$candidate_remote_tag"; }

candidate_local_tag='1111111111111111111111111111111111111111'
: >"$candidate_calls"
release_all_prepare_candidate patch
assert_equal \
  'patch --prepare-only' \
  "$(<"$candidate_calls")" \
  'mismatched release tag does not enter direct resume mode'

candidate_local_tag=''
candidate_remote_tag=''
: >"$candidate_calls"
release_all_prepare_candidate minor
assert_equal \
  'minor --prepare-only' \
  "$(<"$candidate_calls")" \
  'missing release tag uses the checked lower-level recovery path'

candidate_local_tag="$RELEASE_SHA"
release_all_current_release_document() { printf '%s\n' '{"isDraft":null}'; }
: >"$candidate_calls"
set +e
release_all_prepare_candidate patch >/dev/null 2>&1
invalid_draft_exit=$?
set -e
assert_equal '1' "$invalid_draft_exit" 'non-boolean draft state fails closed'

unset -f git
eval "$real_release_require_matching_versions"
eval "$real_release_all_remote_tag_commit"
eval "$real_release_all_current_release_document"
SCRIPT_DIR="$real_script_dir"

if ! /bin/bash -c '
  source "$1"
  RELEASE_ALL_RESUMING=false
  RELEASE_ALL_IS_DRAFT=true
  RELEASE_ALL_SHA=0123456789abcdef0123456789abcdef01234567
  release_all_collect_missing_base_checks() {
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS=()
    RELEASE_ALL_WINDOWS_NEEDED=false
  }
  release_all_collect_missing_candidate_checks() {
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS=()
    RELEASE_ALL_WINDOWS_NEEDED=false
  }
  release_all_run_verification_wave() { [[ "$#" -eq 2 ]]; }
  release_all_require_all_green() { :; }
  release_all_ensure_base_verifications
  release_all_verify_candidate v1.2.3
' _ "$REPO_ROOT/scripts/release-all.sh"; then
  printf '[release-all-tests] FAIL: empty verifier lists failed under system Bash\n' >&2
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
restore_fixture_cleanup_trap
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
restore_fixture_cleanup_trap
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
readiness_marker="$fixture_root/ready"
release_all_run_local_verifiers() {
  trap 'printf terminated >"$termination_marker"; exit 143' TERM
  printf ready >"$readiness_marker"
  while true; do sleep 0.1; done
}
release_all_run_windows_action() {
  local attempt
  for ((attempt = 0; attempt < 100; attempt++)); do
    [[ -e "$readiness_marker" ]] && return 7
    sleep 0.1
  done
  return 8
}
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
