#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=release-common.sh
source "$SCRIPT_DIR/release-common.sh"

verify_linux_docker() {
  if (($# != 8)); then
    release_die "verify_linux_docker requires eight configuration arguments."
  fi

  local docker_platform="$1"
  local rust_target="$2"
  local artifact_platform="$3"
  local container_arch="$4"
  local status_context="$5"
  local image_name="$6"
  local container_name="$7"
  local target_volume="$8"
  local cargo_volume="lfscloud-checks-cargo-cache"
  local container_drive_dir=""
  local drive_config_dir=""
  local existing_container_image=""
  local expected_image=""
  local existing_repo=""
  local existing_drive_source=""
  local existing_user=""
  local host_gid
  local host_uid
  local artifact
  local manifest
  local version
  local rust_version
  local container_exit
  local start_exit
  local status_started=0
  local status_finalized=0
  local -a create_args

  release_initialize "$SCRIPT_DIR"
  cd "$RELEASE_REPO_ROOT"
  release_require_command docker
  release_require_command node
  release_require_command shasum
  release_require_tracked_clean
  release_require_current_commit_on_origin

  if ! docker info >/dev/null 2>&1; then
    release_die "Docker is unavailable. Start the Docker engine and retry."
  fi
  host_uid="$(id -u)"
  host_gid="$(id -g)"

  finalize_linux_status() {
    local exit_code=$?

    trap - EXIT
    if ((status_started == 1 && status_finalized == 0)); then
      release_post_status \
        "$RELEASE_SHA" \
        "$status_context" \
        "failure" \
        "Local Docker $artifact_platform checks failed" \
        || printf 'warning: failed to record the local failure status\n' >&2
    fi
    exit "$exit_code"
  }
  trap finalize_linux_status EXIT

  release_info "Record local Docker verification as pending"
  release_post_status \
    "$RELEASE_SHA" \
    "$status_context" \
    "pending" \
    "Local Docker $artifact_platform checks are running"
  status_started=1

  rust_version="$(release_read_rust_version)"
  if [[ ! "$rust_version" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
    release_die "Cargo.toml package.rust-version is missing or invalid."
  fi

  release_info "Build reusable image $image_name"
  docker build \
    --file "$RELEASE_REPO_ROOT/docker/checks/linux.Dockerfile" \
    --label "com.lfscloud.checks.rust-target=$rust_target" \
    --label "com.lfscloud.checks.rust-version=$rust_version" \
    --platform "$docker_platform" \
    --build-arg "RUST_TARGET=$rust_target" \
    --build-arg "RUST_VERSION=$rust_version" \
    --tag "$image_name" \
    "$RELEASE_REPO_ROOT/docker/checks"

  expected_image="$(docker image inspect --format '{{.Id}}' "$image_name")"
  if [[ -n "${LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR:-}" ]]; then
    drive_config_dir="$LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR"
  elif [[ -f "$RELEASE_REPO_ROOT/.env.local" ]]; then
    drive_config_dir="$(
      sed -n 's/^LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR=//p' \
        "$RELEASE_REPO_ROOT/.env.local" \
        | tail -n 1
    )"
    if [[ "$drive_config_dir" == \"*\" ]] || [[ "$drive_config_dir" == \'*\' ]]; then
      drive_config_dir="${drive_config_dir:1:${#drive_config_dir}-2}"
    fi
  fi
  if [[ -n "$drive_config_dir" ]]; then
    if [[ ! -r "$drive_config_dir/application_default_credentials.json" ]]; then
      release_die "Google Drive config must contain readable application_default_credentials.json."
    fi
    drive_config_dir="$(cd "$drive_config_dir" && pwd)"
    container_drive_dir="/lfscloud-gcloud"
  fi

  if docker container inspect "$container_name" >/dev/null 2>&1; then
    if [[ "$(docker container inspect --format '{{.State.Running}}' "$container_name")" == "true" ]]; then
      release_die "Reusable container $container_name is already running."
    fi
    existing_container_image="$(
      docker container inspect --format '{{.Image}}' "$container_name"
    )"
    existing_repo="$(
      docker container inspect \
        --format '{{ index .Config.Labels "com.lfscloud.checks.repo" }}' \
        "$container_name"
    )"
    existing_drive_source="$(
      docker container inspect \
        --format '{{ index .Config.Labels "com.lfscloud.checks.drive-source" }}' \
        "$container_name"
    )"
    existing_user="$(
      docker container inspect \
        --format '{{ index .Config.Labels "com.lfscloud.checks.user" }}' \
        "$container_name"
    )"
    if [[ "$existing_container_image" != "$expected_image" ]] \
      || [[ "$existing_repo" != "$RELEASE_REPO_ROOT" ]] \
      || [[ "$existing_drive_source" != "$drive_config_dir" ]] \
      || [[ "$existing_user" != "$host_uid:$host_gid" ]]; then
      release_info "Recreate stale container $container_name"
      docker container rm "$container_name" >/dev/null
    fi
  fi

  docker volume create "$cargo_volume" >/dev/null
  docker volume create "$target_volume" >/dev/null
  docker run --rm \
    --platform "$docker_platform" \
    --mount "type=volume,source=$cargo_volume,target=/cargo-cache" \
    --mount "type=volume,source=$target_volume,target=/target" \
    "$image_name" \
    bash -c '
      owner="$1:$2"
      for directory in /cargo-cache /target; do
        if [[ "$(stat -c "%u:%g" "$directory")" != "$owner" ]]; then
          chown -R "$owner" "$directory"
        fi
      done
    ' bash "$host_uid" "$host_gid"
  if ! docker container inspect "$container_name" >/dev/null 2>&1; then
    create_args=(
      create
      --name "$container_name"
      --platform "$docker_platform"
      --label "com.lfscloud.checks.repo=$RELEASE_REPO_ROOT"
      --label "com.lfscloud.checks.drive-source=$drive_config_dir"
      --label "com.lfscloud.checks.user=$host_uid:$host_gid"
      --user "$host_uid:$host_gid"
      --env "CARGO_HOME=/cargo-cache"
      --env "HOME=/tmp/lfscloud-home"
      --env "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR=$container_drive_dir"
      --mount "type=bind,source=$RELEASE_REPO_ROOT,target=/workspace"
      --mount "type=volume,source=$cargo_volume,target=/cargo-cache"
      --mount "type=volume,source=$target_volume,target=/target"
    )
    if [[ -n "$drive_config_dir" ]]; then
      create_args+=(
        --mount "type=bind,source=$drive_config_dir,target=$container_drive_dir"
      )
    fi
    release_info "Create reusable container $container_name"
    docker "${create_args[@]}" \
      "$image_name" \
      /workspace/scripts/docker/run-linux-verification.sh \
      "$rust_target" \
      "$artifact_platform" \
      "$container_arch" >/dev/null
  else
    release_info "Reuse container $container_name"
  fi

  set +e
  docker container start --attach "$container_name"
  start_exit=$?
  set -e
  container_exit="$(
    docker container inspect --format '{{.State.ExitCode}}' "$container_name"
  )"
  if ((start_exit != 0)) || [[ "$container_exit" != "0" ]]; then
    release_die "Docker verification failed in $container_name (exit $container_exit)."
  fi

  if [[ "$(git rev-parse HEAD)" != "$RELEASE_SHA" ]]; then
    release_die "Current commit changed while Docker verification was running."
  fi
  release_require_tracked_clean

  version="$(release_require_matching_versions)"
  artifact="$(release_linux_artifact_path "$version" "$artifact_platform")"
  manifest="$(release_linux_manifest_path "$version" "$artifact_platform")"
  release_verify_checksum "$artifact"
  release_verify_linux_manifest \
    "$artifact" \
    "$manifest" \
    "$version" \
    "$RELEASE_SHA" \
    "$rust_target" \
    "$container_arch"

  release_info "Record local Docker verification as successful"
  release_post_status \
    "$RELEASE_SHA" \
    "$status_context" \
    "success" \
    "Local Docker $artifact_platform checks passed"
  status_finalized=1
  trap - EXIT

  release_pass "Local Docker verification passed for $RELEASE_SHA"
  printf 'Image: %s\n' "$image_name"
  printf 'Container: %s\n' "$container_name"
  printf 'Cargo cache volume: %s\n' "$cargo_volume"
  printf 'Target cache volume: %s\n' "$target_volume"
  printf 'Artifact: %s\n' "$artifact"
}
