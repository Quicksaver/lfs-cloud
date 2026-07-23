#!/usr/bin/env bash

LOCAL_MACOS_STATUS_CONTEXT="local-checks/macos-arm64"
RELEASE_REPO_ROOT=""
RELEASE_BRANCH=""
RELEASE_SHA=""
RELEASE_GITHUB_REPO=""
RELEASE_GITHUB_LOGIN=""
RELEASE_STATUS_STATE=""
RELEASE_STATUS_CREATOR=""
RELEASE_STATUS_DESCRIPTION=""

release_die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

release_info() {
  printf '==> %s\n' "$1"
}

release_pass() {
  printf 'PASS: %s\n' "$1"
}

release_require_command() {
  local command_name="$1"

  if ! command -v "$command_name" >/dev/null 2>&1; then
    release_die "Required command is unavailable: $command_name"
  fi
}

release_initialize() {
  local start_dir="$1"

  release_require_command git
  release_require_command gh
  release_require_command jq

  RELEASE_REPO_ROOT="$(git -C "$start_dir" rev-parse --show-toplevel 2>/dev/null || true)"
  if [[ -z "$RELEASE_REPO_ROOT" ]]; then
    release_die "Could not resolve the repository root."
  fi

  if ! gh auth status --hostname github.com >/dev/null 2>&1; then
    release_die "GitHub CLI is not authenticated. Run 'gh auth login' and retry."
  fi

  RELEASE_GITHUB_REPO="$(
    git -C "$RELEASE_REPO_ROOT" config --get remote.origin.url 2>/dev/null \
      | sed -E \
        -e 's#^git@github\.com:##' \
        -e 's#^ssh://git@github\.com/##' \
        -e 's#^https://github\.com/##' \
        -e 's#^http://github\.com/##' \
        -e 's#^git://github\.com/##' \
        -e 's#\.git$##'
  )"
  if [[ ! "$RELEASE_GITHUB_REPO" =~ ^[^/]+/[^/]+$ ]]; then
    release_die "The origin remote is not a supported GitHub repository URL."
  fi

  RELEASE_GITHUB_LOGIN="$(gh api user --jq '.login' 2>/dev/null || true)"
  if [[ -z "$RELEASE_GITHUB_LOGIN" ]]; then
    release_die "Could not resolve the authenticated GitHub login."
  fi

  RELEASE_BRANCH="$(git -C "$RELEASE_REPO_ROOT" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
  if [[ -z "$RELEASE_BRANCH" ]]; then
    release_die "A local branch must be checked out; detached HEAD is not supported."
  fi

  RELEASE_SHA="$(git -C "$RELEASE_REPO_ROOT" rev-parse HEAD)"
}

release_require_tracked_clean() {
  if ! git -C "$RELEASE_REPO_ROOT" diff --quiet --ignore-submodules --; then
    release_die "Tracked working-tree changes must be committed before continuing."
  fi

  if ! git -C "$RELEASE_REPO_ROOT" diff --cached --quiet --ignore-submodules --; then
    release_die "Staged changes must be committed before continuing."
  fi
}

release_require_fully_clean() {
  local status

  status="$(git -C "$RELEASE_REPO_ROOT" status --porcelain=v1 --untracked-files=all)"
  if [[ -n "$status" ]]; then
    printf '%s\n' "$status" >&2
    release_die "The working tree must be completely clean before continuing."
  fi
}

release_require_current_commit_on_origin() {
  local remote_ref="refs/remotes/origin/$RELEASE_BRANCH"
  local remote_sha
  local github_sha

  release_info "Refresh origin/$RELEASE_BRANCH"
  if ! git -C "$RELEASE_REPO_ROOT" fetch --quiet origin \
    "refs/heads/$RELEASE_BRANCH:$remote_ref"; then
    release_die "Could not fetch origin/$RELEASE_BRANCH."
  fi

  remote_sha="$(git -C "$RELEASE_REPO_ROOT" rev-parse --verify "$remote_ref" 2>/dev/null || true)"
  if [[ "$remote_sha" != "$RELEASE_SHA" ]]; then
    release_die "Current commit $RELEASE_SHA is not exactly origin/$RELEASE_BRANCH (${remote_sha:-missing})."
  fi

  github_sha="$(gh api "repos/$RELEASE_GITHUB_REPO/commits/$RELEASE_SHA" --jq '.sha' 2>/dev/null || true)"
  if [[ "$github_sha" != "$RELEASE_SHA" ]]; then
    release_die "GitHub does not report the current commit for $RELEASE_GITHUB_REPO."
  fi

  release_pass "Current commit is pushed to origin/$RELEASE_BRANCH"
}

release_post_status() {
  local sha="$1"
  local context="$2"
  local state="$3"
  local description="$4"

  gh api --method POST "repos/$RELEASE_GITHUB_REPO/statuses/$sha" \
    --raw-field "state=$state" \
    --raw-field "context=$context" \
    --raw-field "description=$description" \
    --silent
}

release_load_latest_status() {
  local sha="$1"
  local context="$2"
  local record

  record="$(
    gh api "repos/$RELEASE_GITHUB_REPO/commits/$sha/statuses?per_page=100" \
      | jq -c --arg context "$context" \
        '[.[] | select(.context == $context)] | sort_by(.id) | last // empty'
  )"

  if [[ -z "$record" ]]; then
    RELEASE_STATUS_STATE="missing"
    RELEASE_STATUS_CREATOR=""
    RELEASE_STATUS_DESCRIPTION=""
    return
  fi

  RELEASE_STATUS_STATE="$(printf '%s' "$record" | jq -r '.state')"
  RELEASE_STATUS_CREATOR="$(printf '%s' "$record" | jq -r '.creator.login')"
  RELEASE_STATUS_DESCRIPTION="$(printf '%s' "$record" | jq -r '.description // ""')"
}

release_required_status_contexts() {
  printf '%s\n' "$LOCAL_MACOS_STATUS_CONTEXT"
}

release_require_local_statuses_green() {
  local sha="$1"
  local context

  while IFS= read -r context; do
    [[ -n "$context" ]] || continue
    release_load_latest_status "$sha" "$context"

    if [[ "$RELEASE_STATUS_STATE" != "success" ]]; then
      release_die "Required status '$context' is $RELEASE_STATUS_STATE on $sha."
    fi

    if [[ "$RELEASE_STATUS_CREATOR" != "$RELEASE_GITHUB_LOGIN" ]]; then
      release_die "Required status '$context' was set by '$RELEASE_STATUS_CREATOR', not '$RELEASE_GITHUB_LOGIN'."
    fi

    release_pass "$context is green on $sha"
  done <<EOF
$(release_required_status_contexts)
EOF
}

release_read_cargo_version() {
  awk '
    /^\[package\]$/ { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^version = "[^"]+"$/ {
      value = $0
      sub(/^version = "/, "", value)
      sub(/"$/, "", value)
      print value
      exit
    }
  ' "$RELEASE_REPO_ROOT/Cargo.toml"
}

release_read_package_version() {
  node -e '
    const fs = require("node:fs");
    const packageJson = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    process.stdout.write(String(packageJson.version ?? ""));
  ' "$RELEASE_REPO_ROOT/package.json"
}

release_require_matching_versions() {
  local cargo_version
  local package_version

  cargo_version="$(release_read_cargo_version)"
  package_version="$(release_read_package_version)"
  if [[ -z "$cargo_version" ]] || [[ "$cargo_version" != "$package_version" ]]; then
    release_die "Cargo.toml version '$cargo_version' and package.json version '$package_version' must match."
  fi

  printf '%s\n' "$cargo_version"
}

release_next_version() {
  local current="$1"
  local increment="$2"
  local major
  local minor
  local patch

  if [[ ! "$current" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    release_die "Current version is not a supported semantic version: $current"
  fi

  IFS=. read -r major minor patch <<EOF
$current
EOF

  case "$increment" in
    major)
      major=$((major + 1))
      minor=0
      patch=0
      ;;
    minor)
      minor=$((minor + 1))
      patch=0
      ;;
    patch)
      patch=$((patch + 1))
      ;;
    *)
      release_die "Version increment must be major, minor, or patch."
      ;;
  esac

  printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

release_macos_artifact_path() {
  local version="$1"
  printf '%s/dist/lfscloud-v%s-macos-arm64.tar.gz\n' "$RELEASE_REPO_ROOT" "$version"
}

release_macos_manifest_path() {
  local version="$1"
  printf '%s/dist/lfscloud-v%s-macos-arm64.build.json\n' "$RELEASE_REPO_ROOT" "$version"
}

release_verify_checksum() {
  local artifact="$1"
  local checksum="$artifact.sha256"

  if [[ ! -s "$artifact" ]] || [[ ! -s "$checksum" ]]; then
    release_die "Missing release artifact or checksum for $(basename "$artifact")."
  fi

  if ! (
    cd "$(dirname "$artifact")"
    shasum -a 256 --check "$(basename "$checksum")"
  ); then
    release_die "Release artifact checksum validation failed."
  fi
}

release_verify_macos_manifest() {
  local artifact="$1"
  local manifest="$2"
  local version="$3"
  local sha="$4"
  local digest

  if [[ ! -s "$manifest" ]]; then
    release_die "Missing macOS build manifest: $(basename "$manifest")"
  fi

  digest="$(shasum -a 256 "$artifact" | awk 'NR == 1 { print $1 }')"
  if ! jq -e \
    --arg artifact "$(basename "$artifact")" \
    --arg commit "$sha" \
    --arg digest "$digest" \
    --arg version "$version" \
    '
      .schema_version == 1 and
      .artifact == $artifact and
      .commit == $commit and
      .sha256 == $digest and
      .target == "aarch64-apple-darwin" and
      .version == $version and
      (.macos | type == "string" and length > 0) and
      (.rustc | type == "string" and length > 0)
    ' \
    "$manifest" >/dev/null; then
    release_die "macOS build manifest does not match the verified commit and artifact."
  fi
}
