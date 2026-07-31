#!/usr/bin/env bash

RELEASE_COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=terminal-ui.sh
source "$RELEASE_COMMON_DIR/terminal-ui.sh"

LOCAL_MACOS_STATUS_CONTEXT="local-checks/macos-arm64"
LOCAL_LINUX_X86_64_STATUS_CONTEXT="local-checks/linux-x86_64-docker"
LOCAL_LINUX_ARM64_STATUS_CONTEXT="local-checks/linux-arm64-docker"
RELEASE_REPO_ROOT=""
RELEASE_BRANCH=""
RELEASE_SHA=""
RELEASE_GITHUB_REPO=""
RELEASE_GITHUB_LOGIN=""
RELEASE_STATUS_STATE=""
RELEASE_STATUS_CREATOR=""
RELEASE_STATUS_DESCRIPTION=""
RELEASE_UI_INITIALIZED=0

release_ui_initialize() {
  local prefix="$1"
  local section="$2"

  ui_set_prefix "$prefix"
  ui_set_render_mode "task_only"
  ui_init
  ui_set_live_section_running "$section"
  RELEASE_UI_INITIALIZED=1
}

release_ui_finalize() {
  if ((RELEASE_UI_INITIALIZED == 1)); then
    ui_finalize
    RELEASE_UI_INITIALIZED=0
  fi
}

release_run_step() {
  local message="$1"
  local exit_code
  shift

  ui_set_live_task_state "running" "$message"
  if ui_run_with_live_stdout "$@"; then
    ui_set_live_task_state "pass" "$message"
    ui_clear_live_task
    pass "$message"
    return 0
  else
    exit_code=$?
  fi

  ui_set_live_task_state "fail" "$message"
  ui_clear_live_task
  fail "$message"
  return "$exit_code"
}

release_die() {
  ui_clear_live_task
  fail "$1"
  exit 1
}

release_info() {
  info "$1"
}

release_pass() {
  pass "$1"
}

release_warn() {
  warn "$1"
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
    ui_log_persistent_raw_batch "$status" "$YELLOW"
    release_die "The working tree must be completely clean before continuing."
  fi
}

release_require_current_commit_on_origin() {
  local remote_sha
  local github_sha

  if ! remote_sha="$(
    git -C "$RELEASE_REPO_ROOT" ls-remote \
      --heads origin "refs/heads/$RELEASE_BRANCH" \
      | awk 'NR == 1 { print $1 }'
  )"; then
    release_die "Could not read origin/$RELEASE_BRANCH."
  fi

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
  printf '%s\n' \
    "$LOCAL_MACOS_STATUS_CONTEXT" \
    "$LOCAL_LINUX_X86_64_STATUS_CONTEXT" \
    "$LOCAL_LINUX_ARM64_STATUS_CONTEXT"
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

release_read_rust_version() {
  awk '
    /^\[package\]$/ { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^rust-version = "[^"]+"$/ {
      value = $0
      sub(/^rust-version = "/, "", value)
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

release_roll_changelog() {
  local changelog_path="$1"
  local version="$2"
  local release_date="$3"

  node - "$changelog_path" "$version" "$release_date" <<'NODE'
const fs = require("node:fs");

const changelogPath = process.argv[2];
const version = process.argv[3];
const releaseDate = process.argv[4];
const unreleasedHeadingPattern = /^ {0,3}## \[Unreleased\][ \t]*$/gm;
const releaseHeadingPattern = /^ {0,3}## \[(?!Unreleased\])[^\]]+\][^\n]*$/m;

const changelog = fs.readFileSync(changelogPath, "utf8").replace(/\r\n?/g, "\n");
const unreleasedHeadings = [...changelog.matchAll(unreleasedHeadingPattern)];

if (unreleasedHeadings.length !== 1 || unreleasedHeadings[0].index === undefined) {
  throw new Error("CHANGELOG.md must contain exactly one '## [Unreleased]' heading");
}

const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const existingReleasePattern = new RegExp(
  `^ {0,3}## \\[${escapedVersion}\\][^\\n]*$`,
  "m",
);
if (existingReleasePattern.test(changelog)) {
  throw new Error(`CHANGELOG.md already contains a release section for ${version}`);
}

const unreleasedStart = unreleasedHeadings[0].index;
const unreleasedLineEnd = changelog.indexOf("\n", unreleasedStart);
const unreleasedBodyStart =
  unreleasedLineEnd === -1 ? changelog.length : unreleasedLineEnd + 1;
const nextRelease = releaseHeadingPattern.exec(changelog.slice(unreleasedBodyStart));
const unreleasedEnd =
  nextRelease === null
    ? changelog.length
    : unreleasedBodyStart + nextRelease.index;

const preamble = changelog.slice(0, unreleasedStart);
const unreleasedBody = changelog.slice(unreleasedBodyStart, unreleasedEnd).trim();
const releaseBody = unreleasedBody.length === 0 ? "Version bump only." : unreleasedBody;
const releaseHistory = changelog.slice(unreleasedEnd).replace(/^\n+|\n+$/g, "");

let updated = `${preamble}## [Unreleased]\n\n## [${version}] - ${releaseDate}\n\n${releaseBody}`;
if (releaseHistory.length > 0) {
  updated += `\n\n${releaseHistory}`;
}

fs.writeFileSync(changelogPath, `${updated}\n`);
NODE
}

release_extract_changelog_notes() {
  local changelog_path="$1"
  local version="$2"
  local output_path="$3"

  node - "$changelog_path" "$version" "$output_path" <<'NODE'
const fs = require("node:fs");

const changelogPath = process.argv[2];
const version = process.argv[3];
const outputPath = process.argv[4];
const changelog = fs.readFileSync(changelogPath, "utf8").replace(/\r\n?/g, "\n");
const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const releaseHeadingPattern = new RegExp(
  `^ {0,3}## \\[${escapedVersion}\\] - \\d{4}-\\d{2}-\\d{2}[ \\t]*$`,
  "m",
);
const nextReleaseHeadingPattern = /^ {0,3}## \[(?!Unreleased\])[^\]]+\][^\n]*$/m;
const releaseHeading = releaseHeadingPattern.exec(changelog);

if (releaseHeading === null || releaseHeading.index === undefined) {
  throw new Error(`Missing dated CHANGELOG.md release section for ${version}`);
}

const releaseLineEnd = changelog.indexOf("\n", releaseHeading.index);
const releaseBodyStart =
  releaseLineEnd === -1 ? changelog.length : releaseLineEnd + 1;
const nextRelease = nextReleaseHeadingPattern.exec(changelog.slice(releaseBodyStart));
const releaseEnd =
  nextRelease === null ? changelog.length : releaseBodyStart + nextRelease.index;
const releaseBody = changelog.slice(releaseBodyStart, releaseEnd).trim();

if (releaseBody.length === 0) {
  throw new Error(`CHANGELOG.md release notes for ${version} are empty`);
}

fs.writeFileSync(outputPath, `${releaseBody}\n`);
NODE
}

release_macos_artifact_path() {
  local version="$1"
  printf '%s/dist/lfscloud-v%s-macos-arm64.tar.gz\n' "$RELEASE_REPO_ROOT" "$version"
}

release_macos_manifest_path() {
  local version="$1"
  printf '%s/dist/lfscloud-v%s-macos-arm64.build.json\n' "$RELEASE_REPO_ROOT" "$version"
}

release_linux_artifact_path() {
  local version="$1"
  local artifact_platform="$2"
  printf '%s/dist/lfscloud-v%s-%s.tar.gz\n' \
    "$RELEASE_REPO_ROOT" \
    "$version" \
    "$artifact_platform"
}

release_linux_manifest_path() {
  local version="$1"
  local artifact_platform="$2"
  printf '%s/dist/lfscloud-v%s-%s.build.json\n' \
    "$RELEASE_REPO_ROOT" \
    "$version" \
    "$artifact_platform"
}

release_verify_checksum() {
  local artifact="$1"
  local checksum="$artifact.sha256"

  if [[ ! -s "$artifact" ]] || [[ ! -s "$checksum" ]]; then
    release_die "Missing release artifact or checksum for $(basename "$artifact")."
  fi

  if ! (
    cd "$(dirname "$artifact")"
    shasum -a 256 --check "$(basename "$checksum")" >/dev/null
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

release_verify_linux_manifest() {
  local artifact="$1"
  local manifest="$2"
  local version="$3"
  local sha="$4"
  local target="$5"
  local container_arch="$6"
  local digest

  if [[ ! -s "$manifest" ]]; then
    release_die "Missing Linux build manifest: $(basename "$manifest")"
  fi

  digest="$(shasum -a 256 "$artifact" | awk 'NR == 1 { print $1 }')"
  if ! jq -e \
    --arg artifact "$(basename "$artifact")" \
    --arg commit "$sha" \
    --arg container_arch "$container_arch" \
    --arg digest "$digest" \
    --arg target "$target" \
    --arg version "$version" \
    '
      .schema_version == 1 and
      .artifact == $artifact and
      .commit == $commit and
      .sha256 == $digest and
      .target == $target and
      .container_arch == $container_arch and
      .version == $version and
      (.kernel | type == "string" and length > 0) and
      (.rustc | type == "string" and length > 0)
    ' \
    "$manifest" >/dev/null; then
    release_die "Linux build manifest does not match the verified commit and artifact."
  fi
}
