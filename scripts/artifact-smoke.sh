#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 PATH_TO_RELEASE_ARCHIVE [EXPECTED_VERSION]" >&2
  exit 2
fi

archive="$1"
expected_version="${2:-}"
if [ ! -f "$archive" ]; then
  echo "release archive not found: $archive" >&2
  exit 1
fi

work_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$work_dir"
}
trap cleanup EXIT

case "$archive" in
  *.tar.gz)
    tar xzf "$archive" -C "$work_dir"
    binary="$work_dir/yo"
    ;;
  *.zip)
    7z x -y "-o$work_dir" "$archive" >/dev/null
    binary="$work_dir/yo.exe"
    ;;
  *)
    echo "unsupported release archive: $archive" >&2
    exit 1
    ;;
esac

if [ ! -f "$binary" ]; then
  echo "yo binary is missing from $archive" >&2
  exit 1
fi

chmod +x "$binary" 2>/dev/null || true
version_output="$("$binary" --version)"
case "$version_output" in
  yo\ *) ;;
  *)
    echo "unexpected version output: $version_output" >&2
    exit 1
    ;;
esac

if [ -n "$expected_version" ] && [ "$version_output" != "yo $expected_version" ]; then
  echo "artifact version mismatch: expected yo $expected_version, got $version_output" >&2
  exit 1
fi

echo "$version_output"
