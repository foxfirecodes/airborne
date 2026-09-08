#!/usr/bin/env bash
# Creates an unsigned archive. Signing and notarization are external release gates.
set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh --output-dir PATH [--binary PATH]

Packages a macOS release binary as an unsigned, deterministic ZIP plus a
SHA-256 checksum. The script extracts the archive and smoke-tests --version,
--help, and doctor with temporary local state. It does not sign or notarize.
EOF
}

binary="./target/release/airborne"
output_dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary) binary="$2"; shift 2 ;;
    --output-dir) output_dir="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$output_dir" ]] || { echo "--output-dir is required" >&2; exit 2; }
[[ -x "$binary" ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
[[ "$(uname -s)" == "Darwin" ]] || { echo "packaging is supported only on macOS" >&2; exit 2; }

arch="$(uname -m)"
case "$arch" in
  arm64|x86_64) ;;
  *) echo "unsupported macOS architecture: $arch" >&2; exit 2 ;;
esac
version="$($binary --version | awk 'NR == 1 { print $2 }')"
[[ "$version" =~ ^[0-9A-Za-z.+-]+$ ]] || { echo "could not read a safe version from $binary --version" >&2; exit 1; }

mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
archive_name="airborne-${version}-macos-${arch}.zip"
checksum_name="${archive_name}.sha256"
archive="$output_dir/$archive_name"
checksum="$output_dir/$checksum_name"
[[ ! -e "$archive" && ! -e "$checksum" ]] || { echo "refusing to overwrite $archive_name or its checksum" >&2; exit 1; }

work_dir="$(mktemp -d)"
cleanup() { rm -rf "$work_dir"; }
trap cleanup EXIT
package_dir="airborne-${version}-macos-${arch}"
mkdir -p "$work_dir/stage/$package_dir"
install -m 755 "$binary" "$work_dir/stage/$package_dir/airborne"
# ZIP timestamps have a 1980 lower bound. Normalizing it makes repeated output
# stable when the release binary is unchanged.
touch -t 198001010000 "$work_dir/stage/$package_dir/airborne" "$work_dir/stage/$package_dir"

archive_tmp="$work_dir/$archive_name"
(
  cd "$work_dir/stage"
  zip -X -q "$archive_tmp" "$package_dir/airborne"
)
mv "$archive_tmp" "$archive"
digest="$(shasum -a 256 "$archive" | awk '{print $1}')"
printf '%s  %s\n' "$digest" "$archive_name" > "$checksum"

mkdir -p "$work_dir/extract" "$work_dir/data"
unzip -q "$archive" -d "$work_dir/extract"
smoke_binary="$work_dir/extract/$package_dir/airborne"
"$smoke_binary" --version >/dev/null
"$smoke_binary" --help >/dev/null
"$smoke_binary" --data-dir "$work_dir/data" doctor >/dev/null

echo "Created unsigned archive: $archive"
echo "Checksum: $checksum"
