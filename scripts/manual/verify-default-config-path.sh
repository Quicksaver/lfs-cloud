#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

case "$(uname -s)" in
  CYGWIN* | MINGW* | MSYS*)
    is_windows=true
    binary_name="lfscloud.exe"
    ;;
  *)
    is_windows=false
    binary_name="lfscloud"
    ;;
esac

lfscloud_bin="${LFS_CLOUD_SMOKE_BINARY:-}"
if [[ -z "$lfscloud_bin" ]]; then
  (
    cd "$project_dir"
    cargo build --quiet
  )
  target_dir="${CARGO_TARGET_DIR:-$project_dir/target}"
  if [[ "$target_dir" != /* ]] && [[ ! "$target_dir" =~ ^[A-Za-z]:[\\/] ]]; then
    target_dir="$project_dir/$target_dir"
  fi
  lfscloud_bin="$target_dir/debug/$binary_name"
fi

if [[ ! -f "$lfscloud_bin" ]]; then
  echo "compiled LFS Cloud binary does not exist: $lfscloud_bin" >&2
  exit 1
fi

process_path() {
  local path="$1"
  if $is_windows && command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$path"
  else
    printf '%s\n' "$path"
  fi
}

write_config() {
  local path="$1"
  local provider_id="$2"
  cat >"$path" <<YAML
server:
  public_url: http://127.0.0.1:8080
  session_encryption_secret: default-config-smoke-secret-at-least-32-characters

repository_providers:
  $provider_id:
    type: github
    api_url: https://api.github.com
YAML
}

working_dir="$tmp_dir/working-directory"
home_dir="$tmp_dir/home-directory"
mkdir -p "$working_dir" "$home_dir"
write_config "$working_dir/lfscloud.yml" "working-directory-provider"

default_output="$tmp_dir/default-output"
if $is_windows; then
  appdata_dir="$tmp_dir/appdata-directory"
  profile_dir="$tmp_dir/user-profile-directory"
  mkdir -p "$appdata_dir/lfscloud" "$profile_dir"
  write_config "$appdata_dir/lfscloud/config.yml" "appdata-provider"
  write_config "$profile_dir/lfscloud.yml" "legacy-user-profile-provider"
  (
    export APPDATA="$(process_path "$appdata_dir")"
    export USERPROFILE="$(process_path "$profile_dir")"
    cd "$working_dir"
    "$lfscloud_bin" config repository list
  ) >"$default_output" 2>&1

  grep -F "appdata-provider" "$default_output" >/dev/null
  if grep -F "legacy-user-profile-provider" "$default_output" >/dev/null; then
    echo "Windows default config lookup used the legacy USERPROFILE file instead of APPDATA" >&2
    exit 1
  fi
else
  mkdir -p "$home_dir/.config/lfscloud"
  write_config "$home_dir/.config/lfscloud/config.yml" "home-config-provider"
  write_config "$home_dir/lfscloud.yml" "legacy-home-provider"
  (
    export HOME="$(process_path "$home_dir")"
    cd "$working_dir"
    "$lfscloud_bin" config repository list
  ) >"$default_output" 2>&1

  grep -F "home-config-provider" "$default_output" >/dev/null
  if grep -F "legacy-home-provider" "$default_output" >/dev/null; then
    echo "default config lookup used the legacy HOME file instead of HOME/.config" >&2
    exit 1
  fi
fi

if grep -F "working-directory-provider" "$default_output" >/dev/null; then
  echo "default config lookup used the working directory" >&2
  exit 1
fi

missing_home_output="$tmp_dir/missing-home-output"
if (
  unset APPDATA HOME USERPROFILE
  cd "$working_dir"
  "$lfscloud_bin" config repository list
) >"$missing_home_output" 2>&1; then
  echo "default config lookup unexpectedly succeeded without a home directory" >&2
  exit 1
fi
grep -F "cannot resolve the default server config path" "$missing_home_output" >/dev/null

if $is_windows; then
  profile_dir="$tmp_dir/fallback-user-profile-directory"
  mkdir -p "$profile_dir/AppData/Roaming/lfscloud"
  write_config "$profile_dir/AppData/Roaming/lfscloud/config.yml" "user-profile-provider"
  profile_output="$tmp_dir/user-profile-output"
  (
    unset APPDATA HOME
    export USERPROFILE="$(process_path "$profile_dir")"
    cd "$working_dir"
    "$lfscloud_bin" config repository list
  ) >"$profile_output" 2>&1

  grep -F "user-profile-provider" "$profile_output" >/dev/null
  if grep -F "working-directory-provider" "$profile_output" >/dev/null; then
    echo "Windows default config lookup used the working directory instead of USERPROFILE/AppData/Roaming" >&2
    exit 1
  fi
fi

echo "lfscloud default config path verified against the compiled binary"
