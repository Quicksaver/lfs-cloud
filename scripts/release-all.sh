#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/release-common.sh
source "$SCRIPT_DIR/lib/release-common.sh"

RELEASE_ALL_WINDOWS_HOST="${LFS_CLOUD_WINDOWS_SSH_HOST:-windows-desktop}"
RELEASE_ALL_WINDOWS_REPO="${LFS_CLOUD_WINDOWS_REPO:-E:\Projects\lfs-cloud}"
RELEASE_ALL_TAG=""
RELEASE_ALL_SHA=""
RELEASE_ALL_IS_DRAFT=false
RELEASE_ALL_RESUMING=false
RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS=()
RELEASE_ALL_WINDOWS_NEEDED=false
RELEASE_ALL_JOB_PIDS=()

release_all_usage() {
  cat <<'EOF'
Usage: ./scripts/release-all.sh major|minor|patch

Verify the clean synchronized main branch on this Mac and the fleet Windows
desktop, run only missing trusted base checks, prepare the requested version,
verify and assemble its macOS, Linux, and Windows assets concurrently, then
publish and distribute the exact verified release.

Fleet overrides:
  LFS_CLOUD_WINDOWS_SSH_HOST=windows-desktop
  LFS_CLOUD_WINDOWS_REPO=E:\Projects\lfs-cloud
EOF
}

release_all_initialize_ui() {
  release_ui_initialize "[release-all]" "Publish a release across the local fleet"
}

release_all_finalize_ui() {
  release_ui_finalize
}

release_all_validate_windows_repo_path() {
  case "$1" in
    [Ee]:\\*|[Ee]:/*) return 0 ;;
    *)
      printf 'Windows repository must remain on fleet storage E:\\: %s\n' "$1" >&2
      return 1
      ;;
  esac
}

release_all_powershell_literal() {
  local value="$1"
  value="${value//\'/\'\'}"
  printf "'%s'" "$value"
}

release_all_windows_sync_script() {
  local expected_sha="$1"
  local repository
  local expected_sha_literal
  repository="$(release_all_powershell_literal "$RELEASE_ALL_WINDOWS_REPO")"
  expected_sha_literal="$(release_all_powershell_literal "$expected_sha")"

  cat <<EOF
\$ErrorActionPreference = 'Stop'
\$repo = $repository
if (-not (Test-Path -LiteralPath \$repo -PathType Container)) {
    throw "Windows repository does not exist: \$repo"
}
Set-Location -LiteralPath \$repo
\$branch = (& git branch --show-current).Trim()
if (\$LASTEXITCODE -ne 0 -or \$branch -ne 'main') {
    throw "Windows repository must be on main; found '\$branch'."
}
\$status = @(& git status --porcelain=v1 --untracked-files=all)
if (\$LASTEXITCODE -ne 0) { throw 'Could not inspect the Windows working tree.' }
if (\$status.Count -ne 0) { throw 'Windows working tree must be completely clean.' }
& git fetch --quiet origin 'refs/heads/main:refs/remotes/origin/main'
if (\$LASTEXITCODE -ne 0) { throw 'Could not fetch origin/main on Windows.' }
& git merge --ff-only refs/remotes/origin/main
if (\$LASTEXITCODE -ne 0) { throw 'Windows main cannot fast-forward to origin/main.' }
\$sha = (& git rev-parse HEAD).Trim()
if (\$LASTEXITCODE -ne 0 -or \$sha -ne $expected_sha_literal) {
    throw "Windows main is not at the expected release commit."
}
Write-Output \$sha
EOF
}

release_all_windows_verify_script() {
  local repository
  repository="$(release_all_powershell_literal "$RELEASE_ALL_WINDOWS_REPO")"
  cat <<EOF
\$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $repository
& pwsh -NoProfile -File '.\scripts\local\verify-windows.ps1'
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
EOF
}

release_all_windows_release_script() {
  local tag="$1"
  local repository
  local tag_literal
  repository="$(release_all_powershell_literal "$RELEASE_ALL_WINDOWS_REPO")"
  tag_literal="$(release_all_powershell_literal "$tag")"
  cat <<EOF
\$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $repository
& pwsh -NoProfile -File '.\scripts\release.ps1' -Tag $tag_literal
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
EOF
}

release_all_windows_candidate_assets_script() {
  local tag="$1"
  local version="${tag#v}"
  local expected_sha="$2"
  local repository
  local tag_literal
  local version_literal
  local expected_sha_literal
  repository="$(release_all_powershell_literal "$RELEASE_ALL_WINDOWS_REPO")"
  tag_literal="$(release_all_powershell_literal "$tag")"
  version_literal="$(release_all_powershell_literal "$version")"
  expected_sha_literal="$(release_all_powershell_literal "$expected_sha")"

  cat <<EOF
\$ErrorActionPreference = 'Stop'
try {
    \$repo = $repository
    Set-Location -LiteralPath \$repo
    . '.\scripts\release.ps1' -Tag $tag_literal
    Initialize-Release -StartDirectory \$repo
    \$artifact = Get-WindowsArtifactPath -RepositoryRoot \$repo -Version $version_literal
    \$manifest = Get-WindowsManifestPath -RepositoryRoot \$repo -Version $version_literal
    \$assets = @(\$artifact, "\$artifact.sha256", \$manifest)
    if (-not (Test-ArtifactChecksum -ArtifactPath \$artifact)) {
        throw 'Windows candidate artifact checksum is invalid.'
    }
    if (-not (Test-WindowsBuildManifest -ArtifactPath \$artifact -ManifestPath \$manifest -Version $version_literal -Commit $expected_sha_literal)) {
        throw 'Windows candidate build manifest is invalid.'
    }
    \$release = Get-GitHubReleaseDocument -Tag $tag_literal
    if (-not [bool] \$release.isDraft -or \$release.tagName -ne $tag_literal) {
        throw 'Windows candidate release is not the expected draft.'
    }
    Assert-WindowsReleaseAssetsPublished -Release \$release -AssetPaths \$assets
}
catch {
    Write-Error ("Windows candidate assets are not reusable: " + \$_.Exception.Message)
    exit 20
}
EOF
}

release_all_windows_execute_script() {
  local script_text="$1"
  local encoded

  encoded="$(
    printf '%s' "$script_text" \
      | iconv -f UTF-8 -t UTF-16LE \
      | base64 \
      | tr -d '\r\n'
  )"
  ssh -n -o BatchMode=yes -o ConnectTimeout=15 \
    "$RELEASE_ALL_WINDOWS_HOST" \
    "pwsh -NoProfile -NonInteractive -EncodedCommand $encoded"
}

release_all_sync_windows() {
  local expected_sha="$1"
  release_run_step \
    "Fast-forward Windows main to $expected_sha" \
    release_all_windows_execute_script \
    "$(release_all_windows_sync_script "$expected_sha")"
}

release_all_windows_candidate_assets_are_valid() {
  local tag="$1"
  local expected_sha="$2"
  release_all_windows_execute_script \
    "$(release_all_windows_candidate_assets_script "$tag" "$expected_sha")"
}

release_all_status_is_green() {
  local sha="$1"
  local context="$2"

  release_load_latest_status "$sha" "$context"
  [[ "$RELEASE_STATUS_STATE" == success \
    && "$RELEASE_STATUS_CREATOR" == "$RELEASE_GITHUB_LOGIN" ]]
}

release_all_require_all_green() {
  local sha="$1"
  local context

  for context in \
    "$LOCAL_MACOS_STATUS_CONTEXT" \
    "$LOCAL_LINUX_X86_64_STATUS_CONTEXT" \
    "$LOCAL_LINUX_ARM64_STATUS_CONTEXT" \
    "$LOCAL_WINDOWS_STATUS_CONTEXT"; do
    if ! release_all_status_is_green "$sha" "$context"; then
      fail "Required status '$context' is not a trusted success on $sha."
      return 1
    fi
    release_pass "$context is green on $sha"
  done
}

release_all_local_artifacts_are_valid() (
  local environment="$1"
  local version="$2"
  local path

  case "$environment" in
    macos-arm64)
      path="$RELEASE_REPO_ROOT/target/aarch64-apple-darwin/release/lfscloud"
      [[ -x "$path" ]] || return 1
      [[ "$("$path" --version)" == "lfscloud $version" ]] || return 1
      path="$(release_macos_artifact_path "$version")"
      release_verify_checksum "$path"
      release_verify_macos_manifest \
        "$path" \
        "$(release_macos_manifest_path "$version")" \
        "$version" \
        "$RELEASE_ALL_SHA"
      ;;
    linux-x86-64)
      path="$(release_linux_artifact_path "$version" linux-x86_64-musl)"
      release_verify_checksum "$path"
      release_verify_linux_manifest \
        "$path" \
        "$(release_linux_manifest_path "$version" linux-x86_64-musl)" \
        "$version" \
        "$RELEASE_ALL_SHA" \
        x86_64-unknown-linux-musl \
        x86_64
      path="$(release_linux_deb_artifact_path "$version" amd64)"
      release_verify_checksum "$path"
      release_verify_linux_deb_manifest \
        "$path" \
        "$(release_linux_deb_manifest_path "$version" amd64)" \
        "$version" \
        "$RELEASE_ALL_SHA" \
        x86_64-unknown-linux-musl \
        amd64
      ;;
    linux-arm64)
      path="$(release_linux_artifact_path "$version" linux-arm64-musl)"
      release_verify_checksum "$path"
      release_verify_linux_manifest \
        "$path" \
        "$(release_linux_manifest_path "$version" linux-arm64-musl)" \
        "$version" \
        "$RELEASE_ALL_SHA" \
        aarch64-unknown-linux-musl \
        aarch64
      path="$(release_linux_deb_artifact_path "$version" arm64)"
      release_verify_checksum "$path"
      release_verify_linux_deb_manifest \
        "$path" \
        "$(release_linux_deb_manifest_path "$version" arm64)" \
        "$version" \
        "$RELEASE_ALL_SHA" \
        aarch64-unknown-linux-musl \
        arm64
      ;;
    *) return 1 ;;
  esac
)

release_all_prune_logs() {
  local log_root="$RELEASE_REPO_ROOT/logs"
  [[ -d "$log_root" ]] || return 0
  find "$log_root" \
    -type f \
    -path "$log_root/release-*/*.log" \
    -mmin +20160 \
    -delete
  find "$log_root" \
    -depth \
    -type d \
    -name 'release-*' \
    -empty \
    -delete
}

release_all_child_pids() {
  local parent_pid="$1"
  ps -axo pid=,ppid= \
    | awk -v parent_pid="$parent_pid" '$2 == parent_pid { print $1 }'
}

release_all_signal_tree() {
  local signal="$1"
  local pid="$2"
  local child_pid

  while IFS= read -r child_pid; do
    [[ -n "$child_pid" ]] || continue
    release_all_signal_tree "$signal" "$child_pid"
  done < <(release_all_child_pids "$pid")
  kill "-$signal" "$pid" 2>/dev/null || true
}

release_all_stop_jobs() {
  local pid
  for pid in ${RELEASE_ALL_JOB_PIDS[@]+"${RELEASE_ALL_JOB_PIDS[@]}"}; do
    if kill -0 "$pid" 2>/dev/null; then
      release_all_signal_tree TERM "$pid"
    fi
  done
}

release_all_run_local_verifiers() {
  "$SCRIPT_DIR/local/verify-all.sh" "$@"
}

release_all_run_windows_action() {
  local action="$1"
  if [[ "$action" == verify ]]; then
    release_all_windows_execute_script "$(release_all_windows_verify_script)"
  else
    release_all_windows_execute_script "$(release_all_windows_release_script "$RELEASE_ALL_TAG")"
  fi
}

release_all_run_verification_wave() {
  local stage="$1"
  local windows_action="$2"
  shift 2
  local local_environments=("$@")
  local timestamp
  local log_directory
  local local_log=""
  local windows_log=""
  local local_environment_names=""
  local pid
  local exit_code
  local failed=0
  local active
  local idx
  local states=()
  local logs=()
  local labels=()

  timestamp="$(date -u '+%Y%m%dT%H%M%SZ')"
  log_directory="$RELEASE_REPO_ROOT/logs/release-$timestamp-$stage-$$"
  mkdir -p "$log_directory"
  RELEASE_ALL_JOB_PIDS=()

  if ((${#local_environments[@]} > 0)); then
    local_environment_names="${local_environments[*]}"
    local_log="$log_directory/${local_environment_names// /-}.log"
    (
      release_all_run_local_verifiers "${local_environments[@]}"
    ) >"$local_log" 2>&1 &
    RELEASE_ALL_JOB_PIDS+=("$!")
    states+=(running)
    logs+=("$local_log")
    labels+=("Local verification ($local_environment_names)")
  fi

  if [[ "$windows_action" != none ]]; then
    windows_log="$log_directory/windows.log"
    (
      release_all_run_windows_action "$windows_action"
    ) >"$windows_log" 2>&1 &
    RELEASE_ALL_JOB_PIDS+=("$!")
    states+=(running)
    logs+=("$windows_log")
    labels+=("Windows verification")
  fi

  if ((${#RELEASE_ALL_JOB_PIDS[@]} == 0)); then
    release_pass "All $stage verification statuses are already green"
    return 0
  fi

  release_info "Verification logs: $log_directory"
  active=${#RELEASE_ALL_JOB_PIDS[@]}
  while ((active > 0)); do
    for ((idx = 0; idx < ${#RELEASE_ALL_JOB_PIDS[@]}; idx++)); do
      [[ "${states[$idx]}" == running ]] || continue
      pid="${RELEASE_ALL_JOB_PIDS[$idx]}"
      if kill -0 "$pid" 2>/dev/null; then
        continue
      fi

      if wait "$pid"; then
        exit_code=0
      else
        exit_code=$?
      fi
      states[idx]='done'
      active=$((active - 1))
      if ((exit_code != 0)); then
        failed="$exit_code"
        fail "${labels[$idx]} failed (exit $exit_code; log: ${logs[$idx]})"
        release_all_stop_jobs
        break 2
      fi
      release_pass "${labels[$idx]} passed (log: ${logs[$idx]})"
    done
    ((active == 0)) || sleep 0.1
  done

  if ((failed != 0)); then
    for pid in ${RELEASE_ALL_JOB_PIDS[@]+"${RELEASE_ALL_JOB_PIDS[@]}"}; do
      wait "$pid" 2>/dev/null || true
    done
    RELEASE_ALL_JOB_PIDS=()
    return "$failed"
  fi
  RELEASE_ALL_JOB_PIDS=()
}

release_all_collect_missing_base_checks() {
  local sha="$1"
  RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS=()
  RELEASE_ALL_WINDOWS_NEEDED=false

  release_all_status_is_green "$sha" "$LOCAL_MACOS_STATUS_CONTEXT" \
    || RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(macos-arm64)
  release_all_status_is_green "$sha" "$LOCAL_LINUX_ARM64_STATUS_CONTEXT" \
    || RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-arm64)
  release_all_status_is_green "$sha" "$LOCAL_LINUX_X86_64_STATUS_CONTEXT" \
    || RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-x86-64)
  release_all_status_is_green "$sha" "$LOCAL_WINDOWS_STATUS_CONTEXT" \
    || RELEASE_ALL_WINDOWS_NEEDED=true
}

release_all_ensure_base_verifications() {
  local windows_action=none
  if [[ "$RELEASE_ALL_RESUMING" == true ]]; then
    release_pass "Current HEAD is an existing release commit; resume its candidate checks"
    return 0
  fi
  release_all_collect_missing_base_checks "$RELEASE_ALL_SHA"
  if [[ "$RELEASE_ALL_WINDOWS_NEEDED" == true ]]; then
    windows_action=verify
  fi
  release_all_run_verification_wave \
    base "$windows_action" \
    ${RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS[@]+"${RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS[@]}"} \
    || return $?
  release_all_require_all_green "$RELEASE_ALL_SHA"
}

release_all_preflight() {
  local current_version
  local current_tag
  local tag_sha
  release_initialize "$SCRIPT_DIR"
  cd "$RELEASE_REPO_ROOT"
  release_require_command ssh
  release_require_command iconv
  release_require_command base64
  release_require_fully_clean
  if [[ "$RELEASE_BRANCH" != main ]]; then
    fail "All-in-one releases require the local main branch; found $RELEASE_BRANCH."
    return 1
  fi
  release_require_current_commit_on_origin
  release_all_prune_logs
  release_all_validate_windows_repo_path "$RELEASE_ALL_WINDOWS_REPO" || return 1
  RELEASE_ALL_SHA="$RELEASE_SHA"
  release_all_sync_windows "$RELEASE_ALL_SHA" || return $?

  current_version="$(release_require_matching_versions)"
  current_tag="v$current_version"
  tag_sha="$(git rev-list -n 1 "$current_tag" 2>/dev/null || true)"
  if [[ -z "$tag_sha" ]]; then
    tag_sha="$(release_all_remote_tag_commit "$current_tag")" || return $?
  fi
  if [[ "$(git log -1 --format=%s HEAD)" == "Release $current_tag" ]] \
    && [[ "$tag_sha" == "$RELEASE_ALL_SHA" ]]; then
    RELEASE_ALL_RESUMING=true
  else
    RELEASE_ALL_RESUMING=false
  fi
}

release_all_current_release_document() {
  local tag="$1"
  gh release view "$tag" \
    --repo "$RELEASE_GITHUB_REPO" \
    --json isDraft,isImmutable,tagName,url \
    2>/dev/null
}

release_all_remote_tag_commit() {
  local tag="$1"
  local listing
  local commit

  if ! listing="$(
    git ls-remote --tags origin "refs/tags/$tag" "refs/tags/$tag^{}"
  )"; then
    fail "Could not read release tag '$tag' from origin."
    return 1
  fi
  commit="$(
    printf '%s\n' "$listing" \
      | awk -v ref="refs/tags/$tag^{}" '$2 == ref { print $1; exit }'
  )"
  if [[ -z "$commit" ]]; then
    commit="$(
      printf '%s\n' "$listing" \
        | awk -v ref="refs/tags/$tag" '$2 == ref { print $1; exit }'
    )"
  fi
  printf '%s\n' "$commit"
}

release_all_prepare_candidate() {
  local increment="$1"
  local current_version
  local current_tag
  local head_subject
  local tag_sha
  local release_document=""

  current_version="$(release_require_matching_versions)"
  current_tag="v$current_version"
  head_subject="$(git log -1 --format=%s HEAD)"
  tag_sha="$(git rev-list -n 1 "$current_tag" 2>/dev/null || true)"
  if [[ -z "$tag_sha" ]]; then
    tag_sha="$(release_all_remote_tag_commit "$current_tag")"
  fi
  if [[ "$head_subject" == "Release $current_tag" ]] \
    && [[ "$tag_sha" == "$RELEASE_SHA" ]]; then
    release_document="$(release_all_current_release_document "$current_tag" || true)"
    if [[ -z "$release_document" ]]; then
      "$SCRIPT_DIR/release.sh" resume --prepare-only || return $?
      release_document="$(release_all_current_release_document "$current_tag")" || return 1
    fi
    release_pass "Resuming existing release $current_tag"
  else
    "$SCRIPT_DIR/release.sh" "$increment" --prepare-only || return $?
    RELEASE_SHA="$(git rev-parse HEAD)"
    current_version="$(release_require_matching_versions)"
    current_tag="v$current_version"
    release_document="$(release_all_current_release_document "$current_tag")" || return 1
  fi

  RELEASE_ALL_TAG="$current_tag"
  RELEASE_ALL_SHA="$(git rev-parse HEAD)"
  if ! jq -e '.isDraft | type == "boolean"' \
    <<<"$release_document" >/dev/null; then
    fail "GitHub returned an invalid draft-state document for $current_tag."
    return 1
  fi
  RELEASE_ALL_IS_DRAFT="$(jq -r '.isDraft' <<<"$release_document")"
}

release_all_collect_missing_candidate_checks() {
  local version="${RELEASE_ALL_TAG#v}"
  local validation_output
  local validation_exit
  RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS=()
  RELEASE_ALL_WINDOWS_NEEDED=false

  if ! release_all_status_is_green "$RELEASE_ALL_SHA" "$LOCAL_MACOS_STATUS_CONTEXT"; then
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(macos-arm64)
  elif ! release_all_local_artifacts_are_valid macos-arm64 "$version" >/dev/null 2>&1; then
    release_warn "macos-arm64 candidate artifacts are not reusable; rebuild them"
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(macos-arm64)
  fi
  if ! release_all_status_is_green "$RELEASE_ALL_SHA" "$LOCAL_LINUX_ARM64_STATUS_CONTEXT"; then
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-arm64)
  elif ! release_all_local_artifacts_are_valid linux-arm64 "$version" >/dev/null 2>&1; then
    release_warn "linux-arm64 candidate artifacts are not reusable; rebuild them"
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-arm64)
  fi
  if ! release_all_status_is_green "$RELEASE_ALL_SHA" "$LOCAL_LINUX_X86_64_STATUS_CONTEXT"; then
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-x86-64)
  elif ! release_all_local_artifacts_are_valid linux-x86-64 "$version" >/dev/null 2>&1; then
    release_warn "linux-x86-64 candidate artifacts are not reusable; rebuild them"
    RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS+=(linux-x86-64)
  fi
  if ! release_all_status_is_green "$RELEASE_ALL_SHA" "$LOCAL_WINDOWS_STATUS_CONTEXT"; then
    RELEASE_ALL_WINDOWS_NEEDED=true
  elif validation_output="$(
    release_all_windows_candidate_assets_are_valid \
      "$RELEASE_ALL_TAG" "$RELEASE_ALL_SHA" 2>&1
  )"; then
    :
  else
    validation_exit=$?
    validation_output="${validation_output##*$'\n'}"
    if [[ -z "$validation_output" ]]; then
      validation_output="Windows candidate assets are not reusable; rebuild them"
    fi
    if ((validation_exit != 20)); then
      fail "Could not inspect reusable Windows candidate assets: $validation_output"
      return "$validation_exit"
    fi
    release_warn "$validation_output"
    RELEASE_ALL_WINDOWS_NEEDED=true
  fi
}

release_all_verify_candidate() {
  local tag="$1"
  local windows_action=none

  if [[ "$RELEASE_ALL_IS_DRAFT" != true ]]; then
    release_pass "$tag is already published; skip draft verification"
    release_all_require_all_green "$RELEASE_ALL_SHA"
    return
  fi

  release_all_collect_missing_candidate_checks || return $?
  if [[ "$RELEASE_ALL_WINDOWS_NEEDED" == true ]]; then
    windows_action=release
  fi
  release_all_run_verification_wave \
    candidate "$windows_action" \
    ${RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS[@]+"${RELEASE_ALL_MISSING_LOCAL_ENVIRONMENTS[@]}"} \
    || return $?
  release_all_require_all_green "$RELEASE_ALL_SHA"
}

release_all_complete_local_draft() {
  local tag="$1"
  if [[ "$RELEASE_ALL_IS_DRAFT" == true ]]; then
    "$SCRIPT_DIR/release.sh" resume || return $?
    release_pass "Completed macOS and Linux assets for $tag"
  else
    release_pass "$tag is already published; skip draft asset completion"
  fi
}

release_all_publish_candidate() {
  "$SCRIPT_DIR/publish.sh" "$1"
}

release_all_execute() {
  local increment="$1"

  release_all_preflight || return $?
  release_all_ensure_base_verifications || return $?
  release_all_prepare_candidate "$increment" || return $?
  release_all_sync_windows "$RELEASE_ALL_SHA" || return $?
  release_all_verify_candidate "$RELEASE_ALL_TAG" || return $?
  release_all_complete_local_draft "$RELEASE_ALL_TAG" || return $?
  release_all_require_all_green "$RELEASE_ALL_SHA" || return $?
  release_all_publish_candidate "$RELEASE_ALL_TAG"
}

release_all_main() {
  local increment="${1:-}"
  local exit_code=0

  case "$increment" in
    major|minor|patch) ;;
    -h|--help)
      release_all_usage
      return 0
      ;;
    *)
      release_all_usage >&2
      return 2
      ;;
  esac
  if (($# != 1)); then
    release_all_usage >&2
    return 2
  fi

  release_all_initialize_ui
  trap 'release_all_stop_jobs; release_all_finalize_ui' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  release_all_execute "$increment" || exit_code=$?
  trap - EXIT INT TERM
  release_all_stop_jobs
  release_all_finalize_ui
  return "$exit_code"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  release_all_main "$@"
fi
