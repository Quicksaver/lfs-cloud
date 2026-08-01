#!/bin/sh

set -eu

repository="${LFS_CLOUD_GITHUB_REPOSITORY:-Quicksaver/lfs-cloud}"
release_base="${LFS_CLOUD_RELEASE_BASE_URL:-https://github.com/$repository/releases}"
requested_version="${LFS_CLOUD_INSTALL_VERSION:-latest}"
install_dir="${LFS_CLOUD_INSTALL_DIR:-$HOME/.local/bin}"
force=0
dry_run=0
temporary_dir=""

usage() {
  cat <<'EOF'
Usage: lfscloud-installer.sh [OPTIONS]

Install or update a directly managed LFS Cloud executable.

Options:
  --version VERSION     Install a specific semantic version (default: latest)
  --install-dir PATH    Install into PATH (default: $HOME/.local/bin)
  --force               Replace an executable not created by this installer
  --dry-run             Print the resolved operation without changing files
  -h, --help            Show this help

The environment variables LFS_CLOUD_INSTALL_VERSION and
LFS_CLOUD_INSTALL_DIR provide the same version and directory controls.
EOF
}

die() {
  printf 'lfscloud installer: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [ -n "$temporary_dir" ] && [ -d "$temporary_dir" ]; then
    rm -rf -- "$temporary_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || die "--version requires a value"
      requested_version="$2"
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || die "--install-dir requires a value"
      install_dir="$2"
      shift 2
      ;;
    --force)
      force=1
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

if [ "$requested_version" = "latest" ]; then
  latest_url="$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "$release_base/latest")" \
    || die "could not resolve the latest release"
  case "$latest_url" in
    */tag/v*) requested_version="${latest_url##*/tag/v}" ;;
    *) die "latest release did not resolve to a semantic version tag" ;;
  esac
fi

case "$requested_version" in
  '' | *[!0-9.]* | .* | *.) die "invalid semantic version: $requested_version" ;;
esac
old_ifs="$IFS"
IFS=.
set -- $requested_version
IFS="$old_ifs"
[ "$#" -eq 3 ] || die "version must have major.minor.patch form"
for component in "$@"; do
  case "$component" in
    '' | *[!0-9]*) die "version must have major.minor.patch form" ;;
  esac
done

system="$(uname -s)"
machine="$(uname -m)"
case "$system/$machine" in
  Darwin/arm64 | Darwin/aarch64)
    platform="macos-arm64"
    ;;
  Linux/x86_64 | Linux/amd64)
    platform="linux-x86_64-musl"
    ;;
  Linux/aarch64 | Linux/arm64)
    platform="linux-arm64-musl"
    ;;
  *)
    die "unsupported platform: $system/$machine"
    ;;
esac

artifact="lfscloud-v${requested_version}-${platform}.tar.gz"
download_url="$release_base/download/v${requested_version}/$artifact"
target="$install_dir/lfscloud"
receipt="$install_dir/.lfscloud-direct-install"

printf 'LFS Cloud %s for %s/%s\n' "$requested_version" "$system" "$machine"
printf 'Source: %s\n' "$download_url"
printf 'Target: %s\n' "$target"
if [ "$dry_run" -eq 1 ]; then
  exit 0
fi

if [ -e "$target" ] && [ ! -f "$receipt" ] && [ "$force" -ne 1 ]; then
  die "$target already exists and is not managed by this installer; use its package manager or pass --force"
fi

temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-install.XXXXXX")"
archive_path="$temporary_dir/$artifact"
checksum_path="$archive_path.sha256"
curl --proto '=https' --tlsv1.2 -fsSL "$download_url" -o "$archive_path"
curl --proto '=https' --tlsv1.2 -fsSL "$download_url.sha256" -o "$checksum_path"

if command -v sha256sum >/dev/null 2>&1; then
  actual_digest="$(sha256sum "$archive_path" | awk 'NR == 1 { print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  actual_digest="$(shasum -a 256 "$archive_path" | awk 'NR == 1 { print $1 }')"
else
  die "sha256sum or shasum is required"
fi
expected_line="$(tr -d '\r\n' < "$checksum_path")"
[ "$expected_line" = "$actual_digest  $artifact" ] \
  || die "SHA-256 verification failed for $artifact"

if ! tar -tzf "$archive_path" | while IFS= read -r entry; do
  case "$entry" in
    /* | ../* | */../* | */..) exit 1 ;;
  esac
done; then
  die "release archive contains an unsafe path"
fi
tar -xzf "$archive_path" -C "$temporary_dir"
source_binary="$temporary_dir/lfscloud-v${requested_version}-${platform}/lfscloud"
[ -f "$source_binary" ] && [ ! -L "$source_binary" ] \
  || die "release archive does not contain the expected regular executable"
chmod 755 "$source_binary"
[ "$("$source_binary" --version)" = "lfscloud $requested_version" ] \
  || die "downloaded executable reports an unexpected version"

mkdir -p -- "$install_dir"
staged_target="$install_dir/.lfscloud.install.$$"
cp "$source_binary" "$staged_target"
chmod 755 "$staged_target"
mv -f -- "$staged_target" "$target"
printf 'version=%s\nsource=direct\n' "$requested_version" > "$receipt"

printf 'Installed %s\n' "$target"
case ":${PATH:-}:" in
  *:"$install_dir":*) ;;
  *) printf 'Add %s to PATH before running lfscloud.\n' "$install_dir" ;;
esac
