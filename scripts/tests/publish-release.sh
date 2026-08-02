#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../publish.sh
source "$SCRIPT_DIR/../publish.sh"

TESTS_PASSED=0

assert_equal() {
  local expected="$1"
  local actual="$2"
  local message="$3"

  if [[ "$actual" != "$expected" ]]; then
    printf '[publish-release-tests] FAIL: %s (expected %q, got %q)\n' \
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
    printf '[publish-release-tests] FAIL: %s (missing %q)\n' \
      "$message" "$expected" >&2
    exit 1
  fi
  TESTS_PASSED=$((TESTS_PASSED + 1))
}

assert_not_contains() {
  local text="$1"
  local unexpected="$2"
  local message="$3"

  if [[ "$text" == *"$unexpected"* ]]; then
    printf '[publish-release-tests] FAIL: %s (unexpected %q)\n' \
      "$message" "$unexpected" >&2
    exit 1
  fi
  TESTS_PASSED=$((TESTS_PASSED + 1))
}

assert_succeeds() {
  local message="$1"
  shift

  if ! "$@"; then
    printf '[publish-release-tests] FAIL: %s\n' "$message" >&2
    exit 1
  fi
  TESTS_PASSED=$((TESTS_PASSED + 1))
}

status_document='{
  "statuses": [
    {"id": 1, "context": "local-checks/macos-arm64", "state": "failure", "creator": {"login": "fixture"}},
    {"id": 2, "context": "local-checks/macos-arm64", "state": "success", "creator": {"login": "fixture"}}
  ]
}'
assert_equal \
  "success" \
  "$(publish_trusted_status_state "$status_document" "$LOCAL_MACOS_STATUS_CONTEXT" fixture)" \
  "latest trusted status wins"
assert_equal \
  "untrusted" \
  "$(publish_trusted_status_state "$status_document" "$LOCAL_MACOS_STATUS_CONTEXT" other)" \
  "unexpected status creator is rejected"

PUBLISH_TEST_KEYS=(down enter)
PUBLISH_TEST_KEY_INDEX=0
publish_test_read_key() {
  PUBLISH_READ_KEY="${PUBLISH_TEST_KEYS[$PUBLISH_TEST_KEY_INDEX]}"
  PUBLISH_TEST_KEY_INDEX=$((PUBLISH_TEST_KEY_INDEX + 1))
}
publish_test_render() {
  :
}

candidate_document='[
  {"tag":"v2.0.0","version":"2.0.0"},
  {"tag":"v1.0.0","version":"1.0.0"}
]'
publish_read_release_selection \
  "$candidate_document" \
  publish_test_read_key \
  publish_test_render
assert_equal \
  "v1.0.0" \
  "$(jq -r '.tag' <<<"$PUBLISH_SELECTED_CANDIDATE")" \
  "arrow-key selector returns the highlighted candidate"

expected_assets="$(publish_expected_release_asset_names '1.2.3')"
for required in \
  'lfscloud-v1.2.3-windows-x86_64.zip' \
  'lfscloud_1.2.3_amd64.deb' \
  'lfscloud_1.2.3_amd64.build.json' \
  'lfscloud-installer.sh' \
  'lfscloud-installer.ps1.sha256'; do
  assert_contains "$expected_assets" "$required" "expected asset $required is included"
done

formula="$(publish_homebrew_formula_text \
  '1.2.3' \
  "$(printf 'a%.0s' {1..64})" \
  "$(printf 'b%.0s' {1..64})" \
  "$(printf 'c%.0s' {1..64})")"
assert_contains \
  "$formula" \
  'releases/download/v1.2.3/lfscloud-v1.2.3-macos-arm64.tar.gz' \
  "formula uses the versioned release archive"
assert_contains \
  "$formula" \
  'assert_equal "lfscloud #{version}"' \
  "formula verifies the installed binary version"

unset LFS_CLOUD_APT_CLOUDSMITH_TARGET
assert_not_contains \
  "$(publish_distribution_contexts)" \
  "$DISTRIBUTION_APT_CONTEXT" \
  "unset Cloudsmith target removes APT from required distribution contexts"
assert_contains \
  "$(publish_format_candidate '{"tag":"v1.2.3","is_draft":true,"distribution_states":{}}')" \
  'apt:skipped' \
  "selector identifies unconfigured APT publication as skipped"

LFS_CLOUD_APT_CLOUDSMITH_TARGET='fixture/repository/any-distro/any-version'
assert_contains \
  "$(publish_distribution_contexts)" \
  "$DISTRIBUTION_APT_CONTEXT" \
  "configured Cloudsmith target requires APT distribution"
assert_contains \
  "$(publish_format_candidate '{"tag":"v1.2.3","is_draft":true,"distribution_states":{}}')" \
  'apt:missing' \
  "selector retains missing APT status when Cloudsmith is configured"
unset LFS_CLOUD_APT_CLOUDSMITH_TARGET

fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-winget-test.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT

homebrew_step_marker="$fixture_root/homebrew-step"
(
  release_run_step() { printf '%s\n' "$*" >"$homebrew_step_marker"; }
  publish_trust_homebrew_tap 'Quicksaver/tap'
)
assert_contains \
  "$(<"$homebrew_step_marker")" \
  'brew trust --tap Quicksaver/tap' \
  "publisher trusts its configured Homebrew tap before loading the formula"

winget_clone_marker="$fixture_root/winget-clone"
(
  release_run_step() { printf '%s\n' "$*" >"$winget_clone_marker"; }
  publish_clone_winget_fork 'fixture/winget-pkgs' "$fixture_root/winget-checkout"
)
assert_contains \
  "$(<"$winget_clone_marker")" \
  'gh repo clone fixture/winget-pkgs' \
  "publisher clones the selected WinGet fork"
assert_contains \
  "$(<"$winget_clone_marker")" \
  '--no-upstream' \
  "WinGet clone leaves upstream creation to the publisher"

homebrew_formula_source="$fixture_root/expected-lfscloud.rb"
homebrew_tap_fixture="$fixture_root/homebrew-tap"
mkdir -p "$homebrew_tap_fixture/Formula"
git -C "$homebrew_tap_fixture" init --quiet
printf '%s\n' "$formula" >"$homebrew_formula_source"
cp "$homebrew_formula_source" "$homebrew_tap_fixture/Formula/lfscloud.rb"
assert_succeeds \
  "matching generated formula is a resumable dirty Homebrew checkout" \
  publish_homebrew_checkout_is_resumable \
    "$homebrew_tap_fixture" \
    "$homebrew_formula_source"

status_endpoint_marker="$fixture_root/status-endpoint"
RELEASE_GITHUB_REPO='fixture/repository'
gh() {
  printf '%s\n' "$*" >"$status_endpoint_marker"
  printf '%s\n' \
    '[{"id":2,"context":"local-checks/macos-arm64","state":"success","creator":{"login":"fixture"}}]'
}
if ! plural_status_document="$(publish_commit_status_document 'fixture-commit')"; then
  printf '%s\n' \
    '[publish-release-tests] FAIL: plural commit statuses were not normalized' \
    >&2
  exit 1
fi
unset -f gh
assert_equal \
  'api repos/fixture/repository/commits/fixture-commit/statuses?per_page=100' \
  "$(<"$status_endpoint_marker")" \
  "publisher requests the provenance-bearing plural status endpoint"
assert_equal \
  'fixture' \
  "$(jq -r '.statuses[0].creator.login' <<<"$plural_status_document")" \
  "plural status response preserves creator provenance"

clean_guard_marker="$fixture_root/clean-guard-called"
(
  release_ui_initialize() { :; }
  release_ui_finalize() { :; }
  release_initialize() { RELEASE_REPO_ROOT="$fixture_root"; }
  release_require_fully_clean() { : >"$clean_guard_marker"; }
  publish_release_candidates() { printf '[]\n'; }
  release_pass() { :; }
  publish_main
)
if [[ -e "$clean_guard_marker" ]]; then
  printf '%s\n' \
    '[publish-release-tests] FAIL: publisher consulted the current worktree cleanliness' \
    >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

apt_distribution_marker="$fixture_root/apt-distribution-called"
(
  unset LFS_CLOUD_APT_CLOUDSMITH_TARGET
  release_info() { :; }
  publish_distribution_action() { : >"$apt_distribution_marker"; }
  publish_apt_distribution \
    '{"tag":"v1.2.3","commit":"fixture","distribution_states":{}}' \
    "$fixture_root" \
    '1.2.3'
)
if [[ -e "$apt_distribution_marker" ]]; then
  printf '%s\n' \
    '[publish-release-tests] FAIL: unset Cloudsmith target invoked APT distribution' \
    >&2
  exit 1
fi
TESTS_PASSED=$((TESTS_PASSED + 1))

artifact_path="$fixture_root/lfscloud-v1.2.3-linux-x86_64-musl.tar.gz"
manifest_path="$fixture_root/lfscloud-v1.2.3-linux-x86_64-musl.build.json"
printf 'verified artifact bytes' >"$artifact_path"
artifact_digest="$(publish_sha256 "$artifact_path")"
printf '%s  %s\n' "$artifact_digest" "$(basename "$artifact_path")" \
  >"$artifact_path.sha256"
cat >"$manifest_path" <<EOF
{
  "schema_version": 1,
  "artifact": "$(basename "$artifact_path")",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "version": "1.2.3",
  "target": "x86_64-unknown-linux-musl",
  "container_arch": "x86_64",
  "sha256": "$artifact_digest"
}
EOF
assert_succeeds \
  "artifact checksum validates" \
  publish_test_artifact_checksum "$artifact_path"
assert_succeeds \
  "generic manifest validates commit, target, architecture, and digest" \
  publish_test_generic_build_manifest \
    "$artifact_path" \
    "$manifest_path" \
    '1.2.3' \
    '0123456789abcdef0123456789abcdef01234567' \
    '{"target":"x86_64-unknown-linux-musl","container_arch":"x86_64"}'

publish_write_winget_manifests \
  '1.2.3' \
  "$(printf 'd%.0s' {1..64})" \
  "$fixture_root"
installer_manifest="$(<"$fixture_root/Quicksaver.LFSCloud.installer.yaml")"
locale_manifest="$(<"$fixture_root/Quicksaver.LFSCloud.locale.en-US.yaml")"
version_manifest="$(<"$fixture_root/Quicksaver.LFSCloud.yaml")"
assert_contains \
  "$installer_manifest" \
  '# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json' \
  "WinGet installer manifest declares its schema"
assert_contains \
  "$locale_manifest" \
  '# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json' \
  "WinGet locale manifest declares its schema"
assert_contains \
  "$version_manifest" \
  '# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json' \
  "WinGet version manifest declares its schema"
assert_contains \
  "$installer_manifest" \
  'NestedInstallerType: portable' \
  "WinGet manifest uses a portable nested installer"
assert_contains \
  "$installer_manifest" \
  'PortableCommandAlias: lfscloud' \
  "WinGet manifest exposes the lfscloud command"
assert_contains \
  "$installer_manifest" \
  "$(printf 'D%.0s' {1..64})" \
  "WinGet manifest uses the uppercase archive digest"

visible_error_marker="$fixture_root/visible-error"
(
  RELEASE_UI_INITIALIZED=1
  fail() { printf '%s\n' "$1" >"$visible_error_marker"; }
  publish_error 'visible publisher failure'
)
assert_equal \
  'visible publisher failure' \
  "$(<"$visible_error_marker")" \
  "publisher failures use persistent terminal UI output"

publisher_main_definition="$(declare -f publish_main)"
assert_not_contains \
  "$publisher_main_definition" \
  'read -r -p' \
  "selected releases do not require a second typed confirmation"

printf '[publish-release-tests] %d passed, 0 failed\n' "$TESTS_PASSED"
