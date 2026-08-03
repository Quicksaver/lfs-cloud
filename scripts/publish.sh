#!/usr/bin/env bash

set -uo pipefail

PUBLISH_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/release-common.sh
source "$PUBLISH_SCRIPT_DIR/lib/release-common.sh"

DISTRIBUTION_DIRECT_CONTEXT="distribution/direct-installer"
DISTRIBUTION_HOMEBREW_CONTEXT="distribution/homebrew"
DISTRIBUTION_APT_CONTEXT="distribution/apt"
DISTRIBUTION_WINGET_CONTEXT="distribution/winget-submitted"
PUBLISH_READ_KEY=""
PUBLISH_SELECTED_CANDIDATE=""
PUBLISH_TEMPORARY_ROOT=""

publish_usage() {
  cat <<'EOF'
Usage: ./scripts/publish.sh [vX.Y.Z]

List semantic draft releases whose macOS, Linux, and Windows local checks are
green, prompt for one version, verify every remote release asset, enable GitHub
release immutability, publish the draft, and distribute that exact release to:

  - the direct shell and PowerShell installer URLs
  - Quicksaver/homebrew-tap (configurable)
  - optionally, a configured Cloudsmith Debian repository
  - the WinGet Community repository through a manifest pull request

Published immutable releases with incomplete configured distribution statuses
are also listed so interrupted publication can be resumed safely.

Pass a semantic tag to publish or resume that exact eligible release without
opening the interactive selector.

Optional environment:
  LFS_CLOUD_HOMEBREW_TAP_REPO=OWNER/homebrew-TAP
  LFS_CLOUD_APT_CLOUDSMITH_TARGET=OWNER/REPOSITORY/DISTRO/VERSION
EOF
}

publish_error() {
  if ((RELEASE_UI_INITIALIZED == 1)); then
    fail "$1"
  else
    printf '[release-publish] %s\n' "$1" >&2
  fi
}

publish_require_command() {
  local command_name="$1"

  if ! command -v "$command_name" >/dev/null 2>&1; then
    publish_error "Required command is unavailable: $command_name"
    return 1
  fi
}

publish_cleanup() {
  if [[ -n "$PUBLISH_TEMPORARY_ROOT" && -d "$PUBLISH_TEMPORARY_ROOT" ]]; then
    rm -rf -- "$PUBLISH_TEMPORARY_ROOT"
    PUBLISH_TEMPORARY_ROOT=""
  fi
  release_ui_finalize
}

publish_release_document() {
  local tag="$1"
  local document

  if ! document="$(
    gh release view "$tag" \
      --repo "$RELEASE_GITHUB_REPO" \
      --json assets,isDraft,isImmutable,isPrerelease,publishedAt,tagName,url
  )"; then
    publish_error "Could not read GitHub release $tag."
    return 1
  fi
  if ! jq -e . >/dev/null 2>&1 <<<"$document"; then
    publish_error "GitHub returned invalid release JSON for $tag."
    return 1
  fi
  printf '%s\n' "$document"
}

publish_commit_status_document() {
  local commit="$1"
  local document

  if ! document="$(
    gh api "repos/$RELEASE_GITHUB_REPO/commits/$commit/statuses?per_page=100"
  )"; then
    publish_error "Could not read commit statuses for $commit."
    return 1
  fi
  if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$document"; then
    publish_error "GitHub returned invalid commit-status JSON for $commit."
    return 1
  fi
  jq -cn --argjson statuses "$document" '{statuses: $statuses}'
}

publish_trusted_status_state() {
  local document="$1"
  local context="$2"
  local trusted_login="$3"
  local record
  local creator

  record="$(
    jq -c --arg context "$context" \
      '[.statuses[] | select(.context == $context)] | sort_by(.id) | last // empty' \
      <<<"$document"
  )"
  if [[ -z "$record" ]]; then
    printf 'missing\n'
    return
  fi

  creator="$(jq -r '.creator.login // ""' <<<"$record")"
  if [[ "$creator" != "$trusted_login" ]]; then
    printf 'untrusted\n'
    return
  fi
  jq -r '.state' <<<"$record"
}

publish_remote_release_tag_commit() {
  local tag="$1"
  local output
  local direct_commit=""
  local peeled_commit=""
  local commit
  local reference

  if ! output="$(
    git -C "$RELEASE_REPO_ROOT" ls-remote --tags origin \
      "refs/tags/$tag" "refs/tags/$tag^{}"
  )"; then
    publish_error "Could not read release tag $tag from origin."
    return 1
  fi

  while IFS=$'\t' read -r commit reference; do
    case "$reference" in
      "refs/tags/$tag") direct_commit="$commit" ;;
      "refs/tags/$tag^{}") peeled_commit="$commit" ;;
    esac
  done <<<"$output"

  if [[ -n "$peeled_commit" ]]; then
    printf '%s\n' "$peeled_commit"
  else
    printf '%s\n' "$direct_commit"
  fi
}

publish_required_contexts() {
  printf '%s\n' \
    "$LOCAL_MACOS_STATUS_CONTEXT" \
    "$LOCAL_LINUX_X86_64_STATUS_CONTEXT" \
    "$LOCAL_LINUX_ARM64_STATUS_CONTEXT" \
    "$LOCAL_WINDOWS_STATUS_CONTEXT"
}

publish_apt_enabled() {
  [[ -n "${LFS_CLOUD_APT_CLOUDSMITH_TARGET:-}" ]]
}

publish_distribution_contexts() {
  printf '%s\n' \
    "$DISTRIBUTION_DIRECT_CONTEXT" \
    "$DISTRIBUTION_HOMEBREW_CONTEXT"
  if publish_apt_enabled; then
    printf '%s\n' "$DISTRIBUTION_APT_CONTEXT"
  fi
  printf '%s\n' "$DISTRIBUTION_WINGET_CONTEXT"
}

publish_release_candidates() {
  local releases
  local candidates='[]'
  local release
  local tag
  local version
  local commit
  local statuses
  local context
  local state
  local required_are_green
  local distribution_complete
  local distribution_states
  local is_draft
  local is_immutable
  local is_prerelease
  local candidate

  if ! releases="$(
    gh release list \
      --repo "$RELEASE_GITHUB_REPO" \
      --limit 1000 \
      --json tagName,isDraft,isImmutable,isPrerelease,publishedAt
  )"; then
    publish_error "Could not list GitHub releases."
    return 1
  fi
  if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$releases"; then
    publish_error "GitHub returned invalid release-list JSON."
    return 1
  fi

  while IFS= read -r release; do
    tag="$(jq -r '.tagName' <<<"$release")"
    is_prerelease="$(jq -r '.isPrerelease' <<<"$release")"
    if [[ "$is_prerelease" == "true" ]] || [[ ! "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
      continue
    fi
    version="${BASH_REMATCH[1]}"
    if ! commit="$(publish_remote_release_tag_commit "$tag")"; then
      return 1
    fi
    if [[ -z "$commit" ]]; then
      publish_error "Release $tag does not have a matching tag on origin."
      return 1
    fi
    if ! statuses="$(publish_commit_status_document "$commit")"; then
      return 1
    fi

    required_are_green=true
    while IFS= read -r context; do
      state="$(publish_trusted_status_state "$statuses" "$context" "$RELEASE_GITHUB_LOGIN")"
      if [[ "$state" != "success" ]]; then
        required_are_green=false
      fi
    done <<EOF
$(publish_required_contexts)
EOF
    if [[ "$required_are_green" != true ]]; then
      continue
    fi

    distribution_states='{}'
    distribution_complete=true
    while IFS= read -r context; do
      state="$(publish_trusted_status_state "$statuses" "$context" "$RELEASE_GITHUB_LOGIN")"
      distribution_states="$(
        jq -c --arg context "$context" --arg state "$state" \
          '. + {($context): $state}' <<<"$distribution_states"
      )"
      if [[ "$state" != "success" ]]; then
        distribution_complete=false
      fi
    done <<EOF
$(publish_distribution_contexts)
EOF

    is_draft="$(jq -r '.isDraft' <<<"$release")"
    is_immutable="$(jq -r '.isImmutable' <<<"$release")"
    if [[ "$is_draft" != true ]] \
      && { [[ "$is_immutable" != true ]] || [[ "$distribution_complete" == true ]]; }; then
      continue
    fi

    candidate="$(
      jq -cn \
        --arg tag "$tag" \
        --arg version "$version" \
        --arg commit "$commit" \
        --argjson is_draft "$is_draft" \
        --argjson is_immutable "$is_immutable" \
        --argjson distribution_states "$distribution_states" \
        '{
          tag: $tag,
          version: $version,
          commit: $commit,
          is_draft: $is_draft,
          is_immutable: $is_immutable,
          distribution_states: $distribution_states
        }'
    )"
    candidates="$(jq -c --argjson candidate "$candidate" '. + [$candidate]' <<<"$candidates")"
  done < <(jq -c '.[]' <<<"$releases")

  jq -c \
    'sort_by(.version | split(".") | map(tonumber)) | reverse' \
    <<<"$candidates"
}

publish_format_candidate() {
  local candidate="$1"
  local stage
  local apt_state

  if [[ "$(jq -r '.is_draft // false' <<<"$candidate")" == true ]]; then
    stage="draft"
  else
    stage="resume"
  fi
  if publish_apt_enabled; then
    apt_state="$(jq -r --arg context "$DISTRIBUTION_APT_CONTEXT" '.distribution_states[$context] // "missing"' <<<"$candidate")"
  else
    apt_state="skipped"
  fi
  printf '%-12s %-6s direct:%s brew:%s apt:%s winget:%s' \
    "$(jq -r '.tag' <<<"$candidate")" \
    "$stage" \
    "$(jq -r --arg context "$DISTRIBUTION_DIRECT_CONTEXT" '.distribution_states[$context] // "missing"' <<<"$candidate")" \
    "$(jq -r --arg context "$DISTRIBUTION_HOMEBREW_CONTEXT" '.distribution_states[$context] // "missing"' <<<"$candidate")" \
    "$apt_state" \
    "$(jq -r --arg context "$DISTRIBUTION_WINGET_CONTEXT" '.distribution_states[$context] // "missing"' <<<"$candidate")"
}

publish_terminal_read_key() {
  local first=""
  local rest=""

  if ! IFS= read -rsn1 first; then
    PUBLISH_READ_KEY="escape"
    return
  fi
  case "$first" in
    '') PUBLISH_READ_KEY="enter" ;;
    $'\033')
      IFS= read -rsn2 -t 1 rest || true
      case "$rest" in
        '[A') PUBLISH_READ_KEY="up" ;;
        '[B') PUBLISH_READ_KEY="down" ;;
        *) PUBLISH_READ_KEY="escape" ;;
      esac
      ;;
    *) PUBLISH_READ_KEY="other" ;;
  esac
}

publish_terminal_render() {
  local candidates="$1"
  local selected_index="$2"
  local redraw="$3"
  local count
  local index
  local marker
  local candidate

  count="$(jq 'length' <<<"$candidates")"
  if [[ "$redraw" == true ]]; then
    printf '\033[%sA' "$count"
  fi
  index=0
  while ((index < count)); do
    if [[ "$redraw" == true ]]; then
      printf '\r\033[2K'
    fi
    marker=' '
    if ((index == selected_index)); then
      marker='>'
    fi
    candidate="$(jq -c ".[$index]" <<<"$candidates")"
    printf '%s %s\n' "$marker" "$(publish_format_candidate "$candidate")"
    index=$((index + 1))
  done
}

publish_read_release_selection() {
  local candidates="$1"
  local read_key_callback="${2:-publish_terminal_read_key}"
  local render_callback="${3:-publish_terminal_render}"
  local count
  local selected_index=0

  count="$(jq 'length' <<<"$candidates")"
  if ((count == 0)); then
    publish_error "At least one publishable release candidate is required."
    return 1
  fi
  if [[ "$read_key_callback" == "publish_terminal_read_key" ]] \
    && { [[ ! -t 0 ]] || [[ ! -t 1 ]]; }; then
    publish_error "Release publication requires an interactive terminal."
    return 1
  fi

  printf '\nVerified releases ready to publish or resume:\n'
  printf 'Use Up/Down to navigate, Enter to select, or Escape to cancel.\n'
  "$render_callback" "$candidates" "$selected_index" false
  while true; do
    "$read_key_callback"
    case "$PUBLISH_READ_KEY" in
      up)
        selected_index=$(((selected_index - 1 + count) % count))
        "$render_callback" "$candidates" "$selected_index" true
        ;;
      down)
        selected_index=$(((selected_index + 1) % count))
        "$render_callback" "$candidates" "$selected_index" true
        ;;
      enter)
        printf '\n'
        PUBLISH_SELECTED_CANDIDATE="$(jq -c ".[$selected_index]" <<<"$candidates")"
        return
        ;;
      escape)
        printf '\n'
        PUBLISH_SELECTED_CANDIDATE=""
        return
        ;;
    esac
  done
}

publish_select_release_candidate() {
  local candidates="$1"
  local requested_tag="${2:-}"
  local selected

  if [[ -z "$requested_tag" ]]; then
    publish_read_release_selection "$candidates"
    return
  fi
  if [[ ! "$requested_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    publish_error "Release tag must use vX.Y.Z format: $requested_tag"
    return 1
  fi

  selected="$(jq -c --arg tag "$requested_tag" '[.[] | select(.tag == $tag)]' <<<"$candidates")"
  if [[ "$(jq 'length' <<<"$selected")" != 1 ]]; then
    publish_error "Release $requested_tag is not eligible for publication or resumption."
    return 1
  fi
  PUBLISH_SELECTED_CANDIDATE="$(jq -c '.[0]' <<<"$selected")"
}

publish_expected_release_asset_names() {
  local version="$1"
  local artifact
  local manifest

  for artifact in \
    "lfscloud-v$version-macos-arm64.tar.gz" \
    "lfscloud-v$version-linux-x86_64-musl.tar.gz" \
    "lfscloud-v$version-linux-arm64-musl.tar.gz" \
    "lfscloud_${version}_amd64.deb" \
    "lfscloud_${version}_arm64.deb" \
    "lfscloud-v$version-windows-x86_64.zip"; do
    case "$artifact" in
      *.tar.gz) manifest="${artifact%.tar.gz}.build.json" ;;
      *.deb) manifest="${artifact%.deb}.build.json" ;;
      *.zip) manifest="${artifact%.zip}.build.json" ;;
    esac
    printf '%s\n%s.sha256\n%s\n' "$artifact" "$artifact" "$manifest"
  done
  printf '%s\n' \
    'lfscloud-installer.sh' \
    'lfscloud-installer.sh.sha256' \
    'lfscloud-installer.ps1' \
    'lfscloud-installer.ps1.sha256'
}

publish_sha256() {
  shasum -a 256 "$1" | awk 'NR == 1 { print $1 }'
}

publish_file_size() {
  if stat -f '%z' "$1" >/dev/null 2>&1; then
    stat -f '%z' "$1"
  else
    stat -c '%s' "$1"
  fi
}

publish_test_artifact_checksum() {
  local artifact="$1"
  local checksum="$artifact.sha256"
  local digest
  local expected

  [[ -s "$artifact" && -s "$checksum" ]] || return 1
  digest="$(publish_sha256 "$artifact")"
  expected="$(<"$checksum")"
  [[ "$expected" == "$digest  $(basename "$artifact")" ]]
}

publish_test_generic_build_manifest() {
  local artifact="$1"
  local manifest="$2"
  local version="$3"
  local commit="$4"
  local expected_properties="$5"
  local digest

  [[ -s "$artifact" && -s "$manifest" ]] || return 1
  digest="$(publish_sha256 "$artifact")"
  jq -e \
    --arg artifact "$(basename "$artifact")" \
    --arg commit "$commit" \
    --arg digest "$digest" \
    --arg version "$version" \
    --argjson expected "$expected_properties" \
    '
      . as $manifest |
      .schema_version == 1 and
      .artifact == $artifact and
      .commit == $commit and
      .version == $version and
      .sha256 == $digest and
      ($expected | to_entries | all(.[]; ($manifest[.key] | tostring) == (.value | tostring)))
    ' "$manifest" >/dev/null
}

publish_test_windows_build_manifest() {
  local artifact="$1"
  local manifest="$2"
  local version="$3"
  local commit="$4"
  local digest

  [[ -s "$artifact" && -s "$manifest" ]] || return 1
  digest="$(publish_sha256 "$artifact")"
  jq -e \
    --arg artifact "$(basename "$artifact")" \
    --arg commit "$commit" \
    --arg digest "$digest" \
    --arg version "$version" \
    '
      .schema_version == 1 and
      .artifact == $artifact and
      .commit == $commit and
      .version == $version and
      .target == "x86_64-pc-windows-msvc" and
      (.windows | type == "string" and length > 0) and
      (.rustc | type == "string" and length > 0) and
      .sha256 == $digest
    ' "$manifest" >/dev/null
}

publish_verify_downloaded_release_assets() {
  local candidate="$1"
  local release="$2"
  local directory="$3"
  local version
  local commit
  local name
  local path
  local remote_count
  local remote_digest
  local remote_size
  local digest
  local size
  local artifact
  local manifest
  local expected

  version="$(jq -r '.version' <<<"$candidate")"
  commit="$(jq -r '.commit' <<<"$candidate")"
  while IFS= read -r name; do
    path="$directory/$name"
    if [[ ! -s "$path" ]]; then
      publish_error "Downloaded release is missing required asset $name."
      return 1
    fi
    remote_count="$(jq --arg name "$name" '[.assets[] | select(.name == $name)] | length' <<<"$release")"
    if [[ "$remote_count" != 1 ]]; then
      publish_error "GitHub release does not contain exactly one asset named $name."
      return 1
    fi
    digest="$(publish_sha256 "$path")"
    size="$(publish_file_size "$path")"
    remote_digest="$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .digest' <<<"$release")"
    remote_size="$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .size' <<<"$release")"
    if [[ "$remote_digest" != "sha256:$digest" ]] || [[ "$remote_size" != "$size" ]]; then
      publish_error "GitHub release asset $name does not match the downloaded bytes."
      return 1
    fi
  done <<EOF
$(publish_expected_release_asset_names "$version")
EOF

  while IFS='|' read -r artifact expected; do
    path="$directory/$artifact"
    if ! publish_test_artifact_checksum "$path"; then
      publish_error "Release checksum is invalid for $artifact."
      return 1
    fi
    case "$artifact" in
      *.tar.gz) manifest="$directory/${artifact%.tar.gz}.build.json" ;;
      *.deb) manifest="$directory/${artifact%.deb}.build.json" ;;
    esac
    if ! publish_test_generic_build_manifest \
      "$path" "$manifest" "$version" "$commit" "$expected"; then
      publish_error "Build manifest is invalid for $artifact."
      return 1
    fi
  done <<EOF
lfscloud-v$version-macos-arm64.tar.gz|{"target":"aarch64-apple-darwin"}
lfscloud-v$version-linux-x86_64-musl.tar.gz|{"target":"x86_64-unknown-linux-musl","container_arch":"x86_64"}
lfscloud-v$version-linux-arm64-musl.tar.gz|{"target":"aarch64-unknown-linux-musl","container_arch":"aarch64"}
lfscloud_${version}_amd64.deb|{"target":"x86_64-unknown-linux-musl","architecture":"amd64","package_format":"deb"}
lfscloud_${version}_arm64.deb|{"target":"aarch64-unknown-linux-musl","architecture":"arm64","package_format":"deb"}
EOF

  artifact="$directory/lfscloud-v$version-windows-x86_64.zip"
  manifest="$directory/lfscloud-v$version-windows-x86_64.build.json"
  if ! publish_test_artifact_checksum "$artifact" \
    || ! publish_test_windows_build_manifest "$artifact" "$manifest" "$version" "$commit"; then
    publish_error "Windows release checksum or build manifest is invalid."
    return 1
  fi
  for artifact in lfscloud-installer.sh lfscloud-installer.ps1; do
    if ! publish_test_artifact_checksum "$directory/$artifact"; then
      publish_error "Direct installer checksum is invalid for $artifact."
      return 1
    fi
  done
}

publish_homebrew_formula_text() {
  local version="$1"
  local mac_sha256="$2"
  local linux_x64_sha256="$3"
  local linux_arm64_sha256="$4"

  cat <<EOF
class Lfscloud < Formula
  desc "Git LFS-compatible server and CLI for user-controlled storage"
  homepage "https://github.com/Quicksaver/lfs-cloud"
  version "$version"
  license "MIT"

  if OS.mac?
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v$version/lfscloud-v$version-macos-arm64.tar.gz"
    sha256 "$mac_sha256"
    depends_on arch: :arm64
  elsif Hardware::CPU.arm?
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v$version/lfscloud-v$version-linux-arm64-musl.tar.gz"
    sha256 "$linux_arm64_sha256"
  else
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v$version/lfscloud-v$version-linux-x86_64-musl.tar.gz"
    sha256 "$linux_x64_sha256"
  end

  def install
    bin.install "lfscloud"
  end

  test do
    assert_equal "lfscloud #{version}", shell_output("#{bin}/lfscloud --version").strip
  end
end
EOF
}

publish_write_winget_manifests() {
  local version="$1"
  local installer_sha256
  local directory="$3"
  local release_date

  installer_sha256="$(printf '%s' "$2" | tr '[:lower:]' '[:upper:]')"
  release_date="$(date -u +%Y-%m-%d)"
  mkdir -p "$directory" || return 1

  printf '%s\n' \
    '# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json' \
    '' \
    'PackageIdentifier: Quicksaver.LFSCloud' \
    "PackageVersion: $version" \
    'DefaultLocale: en-US' \
    'ManifestType: version' \
    'ManifestVersion: 1.12.0' \
    >"$directory/Quicksaver.LFSCloud.yaml" \
    || return 1

  cat >"$directory/Quicksaver.LFSCloud.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json

PackageIdentifier: Quicksaver.LFSCloud
PackageVersion: $version
InstallerType: zip
NestedInstallerType: portable
Commands:
- lfscloud
ReleaseDate: $release_date
Installers:
- Architecture: x64
  NestedInstallerFiles:
  - RelativeFilePath: lfscloud-v$version-windows-x86_64/lfscloud.exe
    PortableCommandAlias: lfscloud
  InstallerUrl: https://github.com/Quicksaver/lfs-cloud/releases/download/v$version/lfscloud-v$version-windows-x86_64.zip
  InstallerSha256: $installer_sha256
ManifestType: installer
ManifestVersion: 1.12.0
EOF
  if (($? != 0)); then
    return 1
  fi

  cat >"$directory/Quicksaver.LFSCloud.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json

PackageIdentifier: Quicksaver.LFSCloud
PackageVersion: $version
PackageLocale: en-US
Publisher: Quicksaver
PublisherUrl: https://github.com/Quicksaver
PackageName: LFS Cloud
PackageUrl: https://github.com/Quicksaver/lfs-cloud
License: MIT
LicenseUrl: https://github.com/Quicksaver/lfs-cloud/blob/v$version/LICENSE
ShortDescription: Git LFS-compatible server and CLI for user-controlled storage.
Tags:
- git
- git-lfs
- lfs
ReleaseNotesUrl: https://github.com/Quicksaver/lfs-cloud/releases/tag/v$version
ManifestType: defaultLocale
ManifestVersion: 1.12.0
EOF
}

publish_candidate_distribution_state() {
  local candidate="$1"
  local context="$2"

  jq -r --arg context "$context" '.distribution_states[$context] // "missing"' \
    <<<"$candidate"
}

publish_distribution_action() {
  local candidate="$1"
  local context="$2"
  local description="$3"
  local action="$4"
  shift 4
  local commit
  local tag

  commit="$(jq -r '.commit' <<<"$candidate")"
  tag="$(jq -r '.tag' <<<"$candidate")"
  if [[ "$(publish_candidate_distribution_state "$candidate" "$context")" == success ]]; then
    release_pass "$context is already successful for $tag"
    return
  fi

  if ! release_post_status "$commit" "$context" pending "$description is running"; then
    publish_error "Failed to record GitHub commit status '$context' as 'pending'."
    return 1
  fi
  if "$action" "$@"; then
    if ! release_post_status "$commit" "$context" success "$description succeeded"; then
      publish_error "Failed to record GitHub commit status '$context' as 'success'."
      return 1
    fi
    release_pass "$description succeeded"
    return
  fi

  if ! release_post_status "$commit" "$context" failure "$description failed"; then
    release_warn "Could not record failure status for $context."
  fi
  release_warn "$description failed."
  return 1
}

publish_trust_homebrew_tap() {
  local tap_name="$1"

  release_run_step \
    "Trust the configured Homebrew tap" \
    brew trust --tap "$tap_name"
}

publish_homebrew_checkout_is_resumable() {
  local tap_path="$1"
  local formula_path="$2"
  local status

  status="$(git -C "$tap_path" status --porcelain=v1 --untracked-files=all)" \
    || return 1
  case "$status" in
    '?? Formula/lfscloud.rb'|' A Formula/lfscloud.rb'|'A  Formula/lfscloud.rb'|' M Formula/lfscloud.rb'|'M  Formula/lfscloud.rb')
      cmp -s "$formula_path" "$tap_path/Formula/lfscloud.rb"
      ;;
    *)
      return 1
      ;;
  esac
}

publish_homebrew_formula() {
  local formula_path="$1"
  local tag="$2"
  local tap_repository="${LFS_CLOUD_HOMEBREW_TAP_REPO:-Quicksaver/homebrew-tap}"
  local tap_name
  local tap_path
  local formula_directory
  local tap_formula_path
  local status

  publish_require_command brew || return 1
  if [[ ! "$tap_repository" =~ ^([^/]+)/homebrew-([^/]+)$ ]]; then
    publish_error "The Homebrew tap repository must use OWNER/homebrew-TAP form."
    return 1
  fi
  tap_name="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}"
  release_run_step "Register the Homebrew tap" brew tap "$tap_name" || return 1
  if ! tap_path="$(brew --repository "$tap_name")" || [[ -z "$tap_path" ]]; then
    publish_error "Could not resolve the local checkout for Homebrew tap $tap_name."
    return 1
  fi
  if ! status="$(git -C "$tap_path" status --porcelain=v1 --untracked-files=all)"; then
    publish_error "Could not inspect the Homebrew tap checkout."
    return 1
  fi
  if [[ -n "$status" ]]; then
    if ! publish_homebrew_checkout_is_resumable "$tap_path" "$formula_path"; then
      publish_error "Homebrew tap $tap_name contains unrelated local changes."
      return 1
    fi
    release_info "Resuming the matching generated Homebrew formula."
  fi
  release_run_step "Update the Homebrew tap checkout" git -C "$tap_path" pull --ff-only || return 1
  formula_directory="$tap_path/Formula"
  tap_formula_path="$formula_directory/lfscloud.rb"
  mkdir -p "$formula_directory" || return 1
  cp "$formula_path" "$tap_formula_path" || return 1
  publish_trust_homebrew_tap "$tap_name" || return 1
  release_run_step "Check the Homebrew formula style" brew style --formula "$tap_formula_path" || return 1
  release_run_step "Fetch the Homebrew release archive" brew fetch --force --formula "$tap_formula_path" || return 1
  if ! status="$(git -C "$tap_path" status --porcelain=v1)"; then
    publish_error "Could not inspect the Homebrew tap checkout."
    return 1
  fi
  if [[ -z "$status" ]]; then
    return
  fi

  git -C "$tap_path" config user.name 'LFS Cloud Publisher' || return 1
  git -C "$tap_path" config user.email 'support@quicksaver.dev' || return 1
  git -C "$tap_path" add Formula/lfscloud.rb || return 1
  git -C "$tap_path" commit --message "Publish LFS Cloud $tag" || return 1
  git -C "$tap_path" push origin HEAD || return 1
}

publish_debian_packages() {
  local asset_directory="$1"
  local version="$2"
  local architecture

  publish_require_command cloudsmith || return 1
  if [[ -z "${LFS_CLOUD_APT_CLOUDSMITH_TARGET:-}" ]]; then
    publish_error "LFS_CLOUD_APT_CLOUDSMITH_TARGET must identify OWNER/REPOSITORY/DISTRO/VERSION."
    return 1
  fi
  for architecture in amd64 arm64; do
    release_run_step \
      "Publish the Debian $architecture package" \
      cloudsmith push deb "$LFS_CLOUD_APT_CLOUDSMITH_TARGET" \
      "$asset_directory/lfscloud_${version}_${architecture}.deb" --republish \
      || return 1
  done
}

publish_clone_winget_fork() {
  local fork="$1"
  local checkout="$2"

  release_run_step \
    "Clone the WinGet fork metadata" \
    gh repo clone "$fork" "$checkout" --no-upstream -- \
      --filter=blob:none --no-checkout
}

publish_apt_distribution() {
  local candidate="$1"
  local asset_directory="$2"
  local version="$3"
  local tag

  if ! publish_apt_enabled; then
    release_info "APT publication skipped because LFS_CLOUD_APT_CLOUDSMITH_TARGET is unset."
    return
  fi

  tag="$(jq -r '.tag' <<<"$candidate")"
  publish_distribution_action \
    "$candidate" "$DISTRIBUTION_APT_CONTEXT" \
    "APT publication for $tag" \
    publish_debian_packages "$asset_directory" "$version"
}

publish_winget_manifests() {
  local manifest_directory="$1"
  local version="$2"
  local temporary_root="$3"
  local branch="lfscloud-$version"
  local existing
  local fork="$RELEASE_GITHUB_LOGIN/winget-pkgs"
  local default_branch
  local checkout="$temporary_root/winget-pkgs"
  local manifest_relative_path="manifests/q/Quicksaver/LFSCloud/$version"
  local target_directory
  local remote_branch
  local remote_branch_commit=""
  local push_arguments

  existing="$(
    gh pr list --repo microsoft/winget-pkgs --state open \
      --head "$RELEASE_GITHUB_LOGIN:$branch" --json url --jq '.[0].url // empty' \
      2>/dev/null || true
  )"

  if ! gh repo view "$fork" --json name >/dev/null 2>&1; then
    release_run_step \
      "Create the WinGet repository fork" \
      gh repo fork microsoft/winget-pkgs --clone=false --default-branch-only \
      || return 1
  fi
  if ! default_branch="$(
    gh repo view microsoft/winget-pkgs \
      --json defaultBranchRef --jq '.defaultBranchRef.name'
  )" || [[ -z "$default_branch" ]]; then
    publish_error "Could not resolve the WinGet repository default branch."
    return 1
  fi

  publish_clone_winget_fork "$fork" "$checkout" || return 1
  git -C "$checkout" remote add upstream https://github.com/microsoft/winget-pkgs.git || return 1
  git -C "$checkout" sparse-checkout init --no-cone || return 1
  git -C "$checkout" sparse-checkout set "$manifest_relative_path" || return 1
  git -C "$checkout" fetch --depth=1 upstream "$default_branch" || return 1
  git -C "$checkout" switch --create "$branch" "upstream/$default_branch" || return 1

  target_directory="$checkout/$manifest_relative_path"
  mkdir -p "$target_directory" || return 1
  cp "$manifest_directory"/*.yaml "$target_directory/" || return 1
  git -C "$checkout" config user.name 'LFS Cloud Publisher' || return 1
  git -C "$checkout" config user.email 'support@quicksaver.dev' || return 1
  git -C "$checkout" add "$manifest_relative_path" || return 1
  git -C "$checkout" commit \
    --message "New version: Quicksaver.LFSCloud version $version" || return 1

  if ! remote_branch="$(
    git ls-remote "https://github.com/$fork.git" "refs/heads/$branch"
  )"; then
    publish_error "Could not inspect the WinGet publication branch."
    return 1
  fi
  remote_branch_commit="$(printf '%s\n' "$remote_branch" | awk 'NR == 1 { print $1 }')"
  push_arguments=(git -C "$checkout" push --set-upstream)
  if [[ "$remote_branch_commit" =~ ^[0-9a-f]{40}$ ]]; then
    push_arguments+=("--force-with-lease=refs/heads/$branch:$remote_branch_commit")
  fi
  push_arguments+=(origin "HEAD:refs/heads/$branch")
  "${push_arguments[@]}" || return 1

  if [[ -n "$existing" ]]; then
    release_info "Updated existing WinGet pull request: $existing"
    return
  fi

  gh pr create \
    --repo microsoft/winget-pkgs \
    --base "$default_branch" \
    --head "$RELEASE_GITHUB_LOGIN:$branch" \
    --title "New version: Quicksaver.LFSCloud version $version" \
    --body "Automated local submission for Quicksaver.LFSCloud $version." \
    || return 1
}

publish_direct_installers() {
  local asset_directory="$1"
  local tag="$2"
  local installer
  local destination
  local url

  publish_require_command curl || return 1
  for installer in lfscloud-installer.sh lfscloud-installer.ps1; do
    destination="$asset_directory/public-$installer"
    url="https://github.com/$RELEASE_GITHUB_REPO/releases/download/$tag/$installer"
    if ! curl -fsSL "$url" -o "$destination"; then
      publish_error "Could not download published direct installer $installer."
      return 1
    fi
    if [[ "$(publish_sha256 "$destination")" != "$(publish_sha256 "$asset_directory/$installer")" ]]; then
      publish_error "Published direct installer $installer does not match the verified draft asset."
      return 1
    fi
  done
}

publish_main() {
  local requested_tag="${1:-}"
  local candidates
  local candidate
  local tag
  local version
  local is_draft
  local asset_directory
  local release
  local formula_path
  local winget_directory
  local distribution_failed=0
  local exit_code=0

  release_ui_initialize "[release-publish]" "Publish a verified release"
  trap 'publish_cleanup' EXIT
  release_initialize "$PUBLISH_SCRIPT_DIR"
  cd "$RELEASE_REPO_ROOT" || return 1
  publish_require_command shasum || return 1

  if ! candidates="$(publish_release_candidates)"; then
    return 1
  fi
  if [[ "$(jq 'length' <<<"$candidates")" == 0 ]]; then
    if [[ -n "$requested_tag" ]]; then
      publish_error "Release $requested_tag is not eligible for publication or resumption."
      return 1
    fi
    release_pass "No fully verified draft or incomplete immutable release is ready to publish."
    return
  fi
  publish_select_release_candidate "$candidates" "$requested_tag" || return 1
  candidate="$PUBLISH_SELECTED_CANDIDATE"
  if [[ -z "$candidate" ]]; then
    release_info "Release publication cancelled."
    return
  fi

  tag="$(jq -r '.tag' <<<"$candidate")"
  version="$(jq -r '.version' <<<"$candidate")"
  is_draft="$(jq -r '.is_draft' <<<"$candidate")"

  if [[ "$(publish_candidate_distribution_state "$candidate" "$DISTRIBUTION_HOMEBREW_CONTEXT")" != success ]]; then
    publish_require_command brew || return 1
    if ! gh repo view \
      "${LFS_CLOUD_HOMEBREW_TAP_REPO:-Quicksaver/homebrew-tap}" \
      --json nameWithOwner >/dev/null; then
      publish_error "Homebrew tap repository is unavailable: ${LFS_CLOUD_HOMEBREW_TAP_REPO:-Quicksaver/homebrew-tap}"
      return 1
    fi
  fi
  if publish_apt_enabled \
    && [[ "$(publish_candidate_distribution_state "$candidate" "$DISTRIBUTION_APT_CONTEXT")" != success ]]; then
    publish_require_command cloudsmith || return 1
  fi

  if ! PUBLISH_TEMPORARY_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-publish.XXXXXX")"; then
    publish_error "Could not create the temporary publication workspace."
    return 1
  fi
  asset_directory="$PUBLISH_TEMPORARY_ROOT/assets"
  mkdir -p "$asset_directory" || return 1
  release_run_step \
    "Download every draft release asset" \
    gh release download "$tag" --repo "$RELEASE_GITHUB_REPO" --dir "$asset_directory" \
    || return 1
  release="$(publish_release_document "$tag")" || return 1
  if ! publish_verify_downloaded_release_assets "$candidate" "$release" "$asset_directory"; then
    return 1
  fi
  release_pass "Verify checksums, manifests, and remote asset digests"

  formula_path="$PUBLISH_TEMPORARY_ROOT/lfscloud.rb"
  if ! publish_homebrew_formula_text \
    "$version" \
    "$(publish_sha256 "$asset_directory/lfscloud-v$version-macos-arm64.tar.gz")" \
    "$(publish_sha256 "$asset_directory/lfscloud-v$version-linux-x86_64-musl.tar.gz")" \
    "$(publish_sha256 "$asset_directory/lfscloud-v$version-linux-arm64-musl.tar.gz")" \
    >"$formula_path"; then
    publish_error "Could not generate the Homebrew formula."
    return 1
  fi
  winget_directory="$PUBLISH_TEMPORARY_ROOT/winget-manifests"
  publish_write_winget_manifests \
    "$version" \
    "$(publish_sha256 "$asset_directory/lfscloud-v$version-windows-x86_64.zip")" \
    "$winget_directory" \
    || return 1

  if [[ "$is_draft" == true ]]; then
    release_info "Publishing selected verified draft $tag."
    release_run_step \
      "Enable immutable GitHub releases" \
      gh api --method PUT \
        --header 'X-GitHub-Api-Version: 2026-03-10' \
        "repos/$RELEASE_GITHUB_REPO/immutable-releases" \
      || return 1
    release_run_step \
      "Publish immutable release $tag" \
      gh release edit "$tag" --repo "$RELEASE_GITHUB_REPO" --draft=false --latest \
      || return 1
    release="$(publish_release_document "$tag")" || return 1
    if [[ "$(jq -r '.isDraft' <<<"$release")" == true ]] \
      || [[ "$(jq -r '.isImmutable' <<<"$release")" != true ]]; then
      publish_error "GitHub release $tag was not published as immutable."
      return 1
    fi
  fi

  release_run_step \
    "Verify the immutable release attestation for $tag" \
    gh release verify "$tag" --repo "$RELEASE_GITHUB_REPO" \
    || return 1

  publish_distribution_action \
    "$candidate" "$DISTRIBUTION_DIRECT_CONTEXT" \
    "Direct installer publication for $tag" \
    publish_direct_installers "$asset_directory" "$tag" \
    || distribution_failed=1
  publish_distribution_action \
    "$candidate" "$DISTRIBUTION_HOMEBREW_CONTEXT" \
    "Homebrew publication for $tag" \
    publish_homebrew_formula "$formula_path" "$tag" \
    || distribution_failed=1
  publish_apt_distribution \
    "$candidate" "$asset_directory" "$version" \
    || distribution_failed=1
  publish_distribution_action \
    "$candidate" "$DISTRIBUTION_WINGET_CONTEXT" \
    "WinGet submission for $tag" \
    publish_winget_manifests "$winget_directory" "$version" "$PUBLISH_TEMPORARY_ROOT" \
    || distribution_failed=1

  if ((distribution_failed != 0)); then
    publish_error "One or more distribution channels failed; rerun release:publish to resume them."
    exit_code=1
  else
    release_pass "Published and distributed $tag"
  fi
  return "$exit_code"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  if (($# > 1)); then
    publish_error "Expected at most one release tag."
    publish_usage >&2
    exit 2
  fi
  case "${1:-}" in
    -h|--help)
      publish_usage
      exit 0
      ;;
    '')
      publish_main
      ;;
    v*)
      if [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        publish_main "$1"
      else
        publish_error "Unknown argument: $1"
        publish_usage >&2
        exit 2
      fi
      ;;
    *)
      publish_error "Unknown argument: $1"
      publish_usage >&2
      exit 2
      ;;
  esac
fi
