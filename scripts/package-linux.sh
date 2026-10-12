#!/usr/bin/env bash
# Package a Linux build as crusteo-<version>-<target>.tar.gz.
#
#   scripts/package-linux.sh [path/to/crusteo] [out-dir]
#
# Defaults: target/release/crusteo and dist/. TARGET overrides the target
# label in the file name (default: <machine>-unknown-linux-gnu).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${1:-$root/target/release/crusteo}"
out_dir="${2:-$root/dist}"

version="$(awk -F'"' '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{print $2; exit}' "$root/Cargo.toml")"
[ -n "$version" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }
target="${TARGET:-$(uname -m)-unknown-linux-gnu}"
[ -f "$binary" ] || { echo "error: binary not found: $binary" >&2; exit 1; }

name="crusteo-$version-$target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name" "$out_dir"
install -m 0755 "$binary" "$stage/$name/crusteo"
strip "$stage/$name/crusteo" 2>/dev/null || true
for f in README.md LICENSE NOTICE CHANGELOG.md; do
  if [ -f "$root/$f" ]; then cp "$root/$f" "$stage/$name/"; fi
done

out_dir="$(cd "$out_dir" && pwd)"
tar -C "$stage" -czf "$out_dir/$name.tar.gz" "$name"
echo "$out_dir/$name.tar.gz"
