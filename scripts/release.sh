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
  commit and push it, rerun local checks, tag it, and publish its release.

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
      git fetch --quiet origin "refs/tags/$tag:refs/tags/$tag"
    else
      git tag --annotate "$tag" --message "LFS Cloud $tag" "$sha"
    fi
  fi

  if [[ -z "$remote_sha" ]]; then
    git push origin "refs/tags/$tag"
  fi

  remote_sha="$(remote_tag_commit "$tag")"
  if [[ "$remote_sha" != "$sha" ]]; then
    release_die "Remote tag $tag was not created for $sha."
  fi
  release_pass "Tag $tag points to $sha on origin"
}

publish_release() {
  local tag="$1"
  shift
  local assets=("$@")
  local asset
  local asset_name
  local release_json
  local release_url

  if release_json="$(
    gh release view "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --json assets,isDraft,tagName,url \
      2>/dev/null
  )"; then
    if [[ "$(printf '%s' "$release_json" | jq -r '.isDraft')" != "true" ]]; then
      release_die "Release $tag is already published."
    fi

    release_info "Replace assets on the existing draft release $tag"
    gh release upload "$tag" "${assets[@]}" \
      --repo "$RELEASE_GITHUB_REPO" \
      --clobber
  else
    release_info "Create draft release $tag and upload verified assets"
    gh release create "$tag" "${assets[@]}" \
      --repo "$RELEASE_GITHUB_REPO" \
      --draft \
      --verify-tag \
      --generate-notes \
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

  release_info "Publish release $tag"
  gh release edit "$tag" \
    --repo "$RELEASE_GITHUB_REPO" \
    --draft=false \
    --latest

  release_json="$(
    gh release view "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --json isDraft,url
  )"
  if [[ "$(printf '%s' "$release_json" | jq -r '.isDraft')" != "false" ]]; then
    release_die "Release $tag was not published."
  fi

  release_url="$(printf '%s' "$release_json" | jq -r '.url')"
  release_pass "Published $release_url"
}

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

  release_info "Update version $current_version -> $next_version"
  node "$SCRIPT_DIR/lib/update-version.mjs" \
    "$RELEASE_REPO_ROOT" \
    "$current_version" \
    "$next_version"

  cargo metadata --locked --no-deps --format-version 1 >/dev/null
  yarn install --immutable

  changed_files="$(git diff --name-only | LC_ALL=C sort)"
  expected_files="$(printf '%s\n' Cargo.lock Cargo.toml package.json | LC_ALL=C sort)"
  if [[ "$changed_files" != "$expected_files" ]]; then
    printf 'Changed files:\n%s\n' "$changed_files" >&2
    release_die "Version update changed files outside the expected package metadata."
  fi

  git add -- Cargo.toml Cargo.lock package.json
  git commit --message "Release v$next_version"
  release_require_fully_clean

  release_info "Push release commit to origin/$RELEASE_BRANCH"
  git push origin "HEAD:refs/heads/$RELEASE_BRANCH"

  RELEASE_SHA="$(git rev-parse HEAD)"
  release_require_current_commit_on_origin

  release_info "Rerun deterministic local verifications"
  "$SCRIPT_DIR/local/verify-macos.sh"
  "$SCRIPT_DIR/local/verify-linux-arm64.sh"
  "$SCRIPT_DIR/local/verify-linux-x86-64.sh"
  release_require_local_statuses_green "$RELEASE_SHA"

  current_version="$next_version"
else
  tag="v$current_version"
  release_pass "Resuming release $tag from $RELEASE_SHA"
fi

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

linux_arm_artifact="$(
  release_linux_artifact_path "$current_version" "linux-arm64-musl"
)"
linux_arm_manifest="$(
  release_linux_manifest_path "$current_version" "linux-arm64-musl"
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

release_binary="$RELEASE_REPO_ROOT/target/aarch64-apple-darwin/release/lfscloud"
if [[ ! -x "$release_binary" ]]; then
  release_die "Verified release binary is missing: $release_binary"
fi
if [[ "$("$release_binary" --version)" != "lfscloud $current_version" ]]; then
  release_die "Release binary version does not match $current_version."
fi

release_require_local_statuses_green "$RELEASE_SHA"
ensure_release_tag "$tag" "$RELEASE_SHA"
publish_release "$tag" "${release_assets[@]}"
