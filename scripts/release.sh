#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/release-common.sh
source "$SCRIPT_DIR/lib/release-common.sh"

usage() {
  cat <<'EOF'
Usage:
  ./scripts/release.sh major
  ./scripts/release.sh minor
  ./scripts/release.sh patch
  ./scripts/release.sh resume

major, minor, patch
  Require a clean pushed commit with green local checks, increment the version,
  commit and push it, rerun local checks, tag it, and prepare its draft release.
  An untagged current-version release commit resumes without another increment;
  an already-tagged HEAD requires a new commit before another release can start.

resume
  Continue an interrupted release from the current pushed version commit
  without incrementing the version again.
EOF
}

if [[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi
if (($# != 1)); then
  usage >&2
  exit 2
fi

mode="$1"
case "$mode" in
  major | minor | patch | resume) ;;
  *)
    usage >&2
    exit 2
    ;;
esac

release_ui_initialize "[release]" "Prepare an LFS Cloud draft release"
release_notes_file=""
finalize_release() {
  if [[ -n "$release_notes_file" ]] && [[ -f "$release_notes_file" ]]; then
    rm -f -- "$release_notes_file"
  fi
  release_ui_finalize
}
trap finalize_release EXIT

release_initialize "$SCRIPT_DIR"
cd "$RELEASE_REPO_ROOT"

release_require_command node
release_require_command cargo
release_require_command yarn
release_require_command shasum

release_require_fully_clean
release_require_current_commit_on_origin
release_require_local_statuses_green "$RELEASE_SHA"

current_version="$(release_require_matching_versions)"

validate_locked_cargo_metadata() {
  cargo metadata --locked --no-deps --format-version 1 >/dev/null
}

remote_tag_commit() {
  local tag="$1"
  local peeled
  local direct

  peeled="$(
    git ls-remote --tags origin "refs/tags/$tag^{}" \
      | awk 'NR == 1 { print $1 }'
  )"
  if [[ -n "$peeled" ]]; then
    printf '%s\n' "$peeled"
    return
  fi

  direct="$(
    git ls-remote --tags origin "refs/tags/$tag" \
      | awk 'NR == 1 { print $1 }'
  )"
  printf '%s\n' "$direct"
}

ensure_release_tag() {
  local tag="$1"
  local sha="$2"
  local local_sha
  local remote_sha

  local_sha="$(git rev-list -n 1 "$tag" 2>/dev/null || true)"
  remote_sha="$(remote_tag_commit "$tag")"

  if [[ -n "$local_sha" ]] && [[ "$local_sha" != "$sha" ]]; then
    release_die "Local tag $tag points to $local_sha instead of $sha."
  fi
  if [[ -n "$remote_sha" ]] && [[ "$remote_sha" != "$sha" ]]; then
    release_die "Remote tag $tag points to $remote_sha instead of $sha."
  fi

  if [[ -z "$local_sha" ]]; then
    if [[ -n "$remote_sha" ]]; then
      release_run_step \
        "Fetch existing tag $tag" \
        git fetch --quiet origin "refs/tags/$tag:refs/tags/$tag"
    else
      release_run_step \
        "Create annotated tag $tag" \
        git tag --annotate "$tag" --message "LFS Cloud $tag" "$sha"
    fi
  fi

  if [[ -z "$remote_sha" ]]; then
    release_run_step "Push tag $tag" git push origin "refs/tags/$tag"
  fi

  remote_sha="$(remote_tag_commit "$tag")"
  if [[ "$remote_sha" != "$sha" ]]; then
    release_die "Remote tag $tag was not created for $sha."
  fi
  release_pass "Tag $tag points to $sha on origin"
}

prepare_release_draft() {
  local tag="$1"
  local notes_file="$2"
  shift 2
  local assets=("$@")
  local asset
  local asset_name
  local release_json

  if release_json="$(
    gh release view "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --json assets,isDraft,tagName,url \
      2>/dev/null
  )"; then
    if [[ "$(printf '%s' "$release_json" | jq -r '.isDraft')" != "true" ]]; then
      release_die "Release $tag is already published."
    fi

    release_run_step \
      "Update notes on the existing draft release $tag" \
      gh release edit "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --notes-file "$notes_file"

    release_run_step \
      "Replace assets on the existing draft release $tag" \
      gh release upload "$tag" "${assets[@]}" \
      --repo "$RELEASE_GITHUB_REPO" \
      --clobber
  else
    release_run_step \
      "Create draft release $tag and upload verified assets" \
      gh release create "$tag" "${assets[@]}" \
      --repo "$RELEASE_GITHUB_REPO" \
      --draft \
      --verify-tag \
      --notes-file "$notes_file" \
      --title "LFS Cloud $tag"
  fi

  release_json="$(
    gh release view "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --json assets,isDraft,tagName,url
  )"
  if [[ "$(printf '%s' "$release_json" | jq -r '.isDraft')" != "true" ]]; then
    release_die "Release $tag must remain a draft until its assets are verified."
  fi

  for asset in "${assets[@]}"; do
    asset_name="$(basename "$asset")"
    if ! printf '%s' "$release_json" \
      | jq -e --arg name "$asset_name" \
        '[.assets[] | select(.name == $name and .state == "uploaded" and .size > 0)] | length == 1' \
        >/dev/null; then
      release_die "Draft release $tag does not contain uploaded asset $asset_name."
    fi
  done

  release_pass "Prepared draft $(printf '%s' "$release_json" | jq -r '.url')"
}

run_all_local_verifiers() {
  local verify_exit

  ui_clear_live_state
  set +e
  "$SCRIPT_DIR/local/verify-all.sh"
  verify_exit=$?
  set -e
  ui_set_live_section_running "Prepare an LFS Cloud draft release"

  if ((verify_exit != 0)); then
    release_die "Local release verification failed."
  fi
  release_pass "All local release environments passed"
}

current_tag="v$current_version"
head_subject="$(git log -1 --format=%s "$RELEASE_SHA")"
local_current_tag_sha="$(git rev-list -n 1 "$current_tag" 2>/dev/null || true)"
remote_current_tag_sha="$(remote_tag_commit "$current_tag")"
version_action="$(
  release_classify_version_action \
    "$mode" \
    "$current_version" \
    "$head_subject" \
    "$RELEASE_SHA" \
    "$local_current_tag_sha" \
    "$remote_current_tag_sha"
)"
case "$version_action" in
  increment) ;;
  resume)
    if [[ "$mode" != "resume" ]]; then
      release_pass \
        "Detected untagged release commit $current_tag; resume without another version increment"
      mode="resume"
    fi
    ;;
  already-released)
    release_die \
      "Current HEAD is already released as $current_tag; commit new changes before requesting another version increment."
    ;;
  conflict)
    release_die \
      "Release tag $current_tag does not consistently identify current HEAD $RELEASE_SHA."
    ;;
  *)
    release_die "Unsupported release version action: $version_action"
    ;;
esac

if [[ "$mode" != "resume" ]]; then
  next_version="$(release_next_version "$current_version" "$mode")"
  tag="v$next_version"

  if git show-ref --verify --quiet "refs/tags/$tag"; then
    release_die "Local tag already exists: $tag"
  fi
  if [[ -n "$(remote_tag_commit "$tag")" ]]; then
    release_die "Remote tag already exists: $tag"
  fi
  if gh release view "$tag" --repo "$RELEASE_GITHUB_REPO" >/dev/null 2>&1; then
    release_die "GitHub release already exists: $tag"
  fi

  release_run_step \
    "Update version $current_version -> $next_version" \
    node "$SCRIPT_DIR/lib/update-version.mjs" \
    "$RELEASE_REPO_ROOT" \
    "$current_version" \
    "$next_version"

  release_run_step \
    "Roll Unreleased changelog entries into $next_version" \
    release_roll_changelog \
    "$RELEASE_REPO_ROOT/CHANGELOG.md" \
    "$next_version" \
    "$(date '+%Y-%m-%d')"

  release_run_step \
    "Validate locked Cargo metadata" \
    validate_locked_cargo_metadata
  release_run_step "Validate Yarn install state" yarn install --immutable

  changed_files="$(git diff --name-only | LC_ALL=C sort)"
  expected_files="$(printf '%s\n' CHANGELOG.md Cargo.lock Cargo.toml package.json | LC_ALL=C sort)"
  if [[ "$changed_files" != "$expected_files" ]]; then
    ui_log_persistent_raw_batch "Changed files:
$changed_files" "$YELLOW"
    release_die "Version update changed files outside the expected package metadata."
  fi

  git add -- CHANGELOG.md Cargo.toml Cargo.lock package.json
  release_run_step \
    "Commit release v$next_version" \
    git commit --message "Release v$next_version"
  release_require_fully_clean

  release_run_step \
    "Push release commit to origin/$RELEASE_BRANCH" \
    git push origin "HEAD:refs/heads/$RELEASE_BRANCH"

  RELEASE_SHA="$(git rev-parse HEAD)"
  release_require_current_commit_on_origin

  run_all_local_verifiers
  release_require_local_statuses_green "$RELEASE_SHA"

  current_version="$next_version"
else
  tag="v$current_version"
  release_pass "Resuming release $tag from $RELEASE_SHA"
fi

published_version="$(release_latest_published_version)"
if [[ -n "$published_version" ]]; then
  release_pass "Latest published semantic release is v$published_version"
else
  release_pass "No published semantic release exists; include all recorded release notes"
fi
release_notes_file="$(mktemp "${TMPDIR:-/tmp}/lfscloud-release-notes.XXXXXX")"
release_run_step \
  "Extract cumulative unpublished release notes through $current_version" \
  release_extract_cumulative_changelog_notes \
  "$RELEASE_REPO_ROOT/CHANGELOG.md" \
  "$current_version" \
  "$published_version" \
  "$release_notes_file"

artifact="$(release_macos_artifact_path "$current_version")"
manifest="$(release_macos_manifest_path "$current_version")"
release_verify_checksum "$artifact"
release_verify_macos_manifest "$artifact" "$manifest" "$current_version" "$RELEASE_SHA"
release_assets=("$artifact" "$artifact.sha256" "$manifest")

linux_x86_artifact="$(
  release_linux_artifact_path "$current_version" "linux-x86_64-musl"
)"
linux_x86_manifest="$(
  release_linux_manifest_path "$current_version" "linux-x86_64-musl"
)"
linux_x86_deb="$(release_linux_deb_artifact_path "$current_version" "amd64")"
linux_x86_deb_manifest="$(
  release_linux_deb_manifest_path "$current_version" "amd64"
)"
release_verify_checksum "$linux_x86_artifact"
release_verify_linux_manifest \
  "$linux_x86_artifact" \
  "$linux_x86_manifest" \
  "$current_version" \
  "$RELEASE_SHA" \
  "x86_64-unknown-linux-musl" \
  "x86_64"
release_assets+=(
  "$linux_x86_artifact"
  "$linux_x86_artifact.sha256"
  "$linux_x86_manifest"
)
release_verify_checksum "$linux_x86_deb"
release_verify_linux_deb_manifest \
  "$linux_x86_deb" \
  "$linux_x86_deb_manifest" \
  "$current_version" \
  "$RELEASE_SHA" \
  "x86_64-unknown-linux-musl" \
  "amd64"
release_assets+=(
  "$linux_x86_deb"
  "$linux_x86_deb.sha256"
  "$linux_x86_deb_manifest"
)

linux_arm_artifact="$(
  release_linux_artifact_path "$current_version" "linux-arm64-musl"
)"
linux_arm_manifest="$(
  release_linux_manifest_path "$current_version" "linux-arm64-musl"
)"
linux_arm_deb="$(release_linux_deb_artifact_path "$current_version" "arm64")"
linux_arm_deb_manifest="$(
  release_linux_deb_manifest_path "$current_version" "arm64"
)"
release_verify_checksum "$linux_arm_artifact"
release_verify_linux_manifest \
  "$linux_arm_artifact" \
  "$linux_arm_manifest" \
  "$current_version" \
  "$RELEASE_SHA" \
  "aarch64-unknown-linux-musl" \
  "aarch64"
release_assets+=(
  "$linux_arm_artifact"
  "$linux_arm_artifact.sha256"
  "$linux_arm_manifest"
)
release_verify_checksum "$linux_arm_deb"
release_verify_linux_deb_manifest \
  "$linux_arm_deb" \
  "$linux_arm_deb_manifest" \
  "$current_version" \
  "$RELEASE_SHA" \
  "aarch64-unknown-linux-musl" \
  "arm64"
release_assets+=(
  "$linux_arm_deb"
  "$linux_arm_deb.sha256"
  "$linux_arm_deb_manifest"
)

stage_direct_installer() {
  local source_path="$1"
  local destination_path="$2"

  cp "$source_path" "$destination_path"
  (
    cd "$(dirname "$destination_path")"
    shasum -a 256 "$(basename "$destination_path")" \
      > "$(basename "$destination_path").sha256"
  )
  release_verify_checksum "$destination_path"
}

mkdir -p "$RELEASE_REPO_ROOT/dist"
shell_installer="$RELEASE_REPO_ROOT/dist/lfscloud-installer.sh"
powershell_installer="$RELEASE_REPO_ROOT/dist/lfscloud-installer.ps1"
release_run_step \
  "Stage the direct-install scripts" \
  stage_direct_installer \
  "$SCRIPT_DIR/install.sh" \
  "$shell_installer"
release_run_step \
  "Stage the PowerShell direct installer" \
  stage_direct_installer \
  "$SCRIPT_DIR/install.ps1" \
  "$powershell_installer"
release_assets+=(
  "$shell_installer"
  "$shell_installer.sha256"
  "$powershell_installer"
  "$powershell_installer.sha256"
)

release_binary="$RELEASE_REPO_ROOT/target/aarch64-apple-darwin/release/lfscloud"
if [[ ! -x "$release_binary" ]]; then
  release_die "Verified release binary is missing: $release_binary"
fi
if [[ "$("$release_binary" --version)" != "lfscloud $current_version" ]]; then
  release_die "Release binary version does not match $current_version."
fi

release_require_local_statuses_green "$RELEASE_SHA"
ensure_release_tag "$tag" "$RELEASE_SHA"
prepare_release_draft "$tag" "$release_notes_file" "${release_assets[@]}"
