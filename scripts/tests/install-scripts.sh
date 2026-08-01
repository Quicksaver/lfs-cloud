#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/lfscloud-installer-tests.XXXXXX")"
cleanup() {
  rm -rf -- "$fixture_root"
}
trap cleanup EXIT

version="1.2.3"
platform="macos-arm64"
artifact="lfscloud-v${version}-${platform}.tar.gz"
release_files="$fixture_root/release-files"
package_root="$fixture_root/package/lfscloud-v${version}-${platform}"
fake_bin="$fixture_root/bin"
install_dir="$fixture_root/install"
mkdir -p "$release_files" "$package_root" "$fake_bin"

cat > "$package_root/lfscloud" <<'EOF'
#!/bin/sh
printf 'lfscloud 1.2.3\n'
EOF
chmod +x "$package_root/lfscloud"
tar -czf "$release_files/$artifact" -C "$fixture_root/package" "lfscloud-v${version}-${platform}"
digest="$(shasum -a 256 "$release_files/$artifact" | awk 'NR == 1 { print $1 }')"
printf '%s  %s\n' "$digest" "$artifact" > "$release_files/$artifact.sha256"

cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf 'Darwin\n' ;;
  -m) printf 'arm64\n' ;;
  *) exit 2 ;;
esac
EOF
cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
set -eu
output=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      output="$2"
      shift 2
      ;;
    --proto | -w)
      shift 2
      ;;
    --tlsv1.2)
      shift
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
if [ "$url" != "${url%/latest}" ]; then
  printf '%s/tag/v%s' "${url%/latest}" "$LFS_CLOUD_FAKE_VERSION"
  exit 0
fi
[ -n "$output" ] || exit 2
cp "$LFS_CLOUD_FAKE_RELEASE_FILES/${url##*/}" "$output"
EOF
chmod +x "$fake_bin/uname" "$fake_bin/curl"

PATH="$fake_bin:$PATH" \
  LFS_CLOUD_FAKE_RELEASE_FILES="$release_files" \
  LFS_CLOUD_FAKE_VERSION="$version" \
  LFS_CLOUD_INSTALL_VERSION="$version" \
  LFS_CLOUD_INSTALL_DIR="$install_dir" \
  "$REPO_ROOT/scripts/install.sh" >/dev/null

if [[ "$($install_dir/lfscloud --version)" != "lfscloud $version" ]]; then
  printf 'installed executable reported an unexpected version\n' >&2
  exit 1
fi
if ! grep -q '^source=direct$' "$install_dir/.lfscloud-direct-install"; then
  printf 'direct-install receipt was not written\n' >&2
  exit 1
fi

PATH="$fake_bin:$PATH" \
  LFS_CLOUD_FAKE_RELEASE_FILES="$release_files" \
  LFS_CLOUD_FAKE_VERSION="$version" \
  LFS_CLOUD_INSTALL_VERSION="latest" \
  LFS_CLOUD_INSTALL_DIR="$install_dir" \
  "$REPO_ROOT/scripts/install.sh" >/dev/null
if [[ "$($install_dir/lfscloud --version)" != "lfscloud $version" ]]; then
  printf 'latest-version update installed an unexpected version\n' >&2
  exit 1
fi

rm -f -- "$install_dir/.lfscloud-direct-install"
if PATH="$fake_bin:$PATH" \
  LFS_CLOUD_FAKE_RELEASE_FILES="$release_files" \
  LFS_CLOUD_FAKE_VERSION="$version" \
  LFS_CLOUD_INSTALL_VERSION="$version" \
  LFS_CLOUD_INSTALL_DIR="$install_dir" \
  "$REPO_ROOT/scripts/install.sh" >/dev/null 2>&1; then
  printf 'installer replaced an unmanaged executable without --force\n' >&2
  exit 1
fi

printf '[install-script-tests] 4 passed, 0 failed\n'
