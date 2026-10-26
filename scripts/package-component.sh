#!/usr/bin/env bash
# Package reef or burrow as <name>-<version>-<target>.tar.gz: the
# binary, its install guide as INSTALL.md, and the example service files from
# packaging/<component>/ in examples/.
#
#   scripts/package-component.sh <reef|burrow> <path/to/binary> <target> [out-dir]
#
# <target> labels the file name, such as x86_64-unknown-linux-musl or
# macos-universal. out-dir defaults to dist/. STRIP names the strip to use
# (default: strip, or aarch64-linux-gnu-strip for an aarch64 Linux binary
# built elsewhere).
set -euo pipefail

usage() { echo "usage: $0 <reef|burrow> <path/to/binary> <target> [out-dir]" >&2; exit 2; }

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ $# -ge 3 ] && [ $# -le 4 ] || usage
component="$1"
binary="$2"
target="$3"
out_dir="${4:-$root/dist}"

case "$component" in
  reef) name=reef; guide=docs/reef/install.md ;;
  burrow) name=burrow; guide=docs/burrow/install.md ;;
  *) usage ;;
esac

version="$(awk -F'"' '/^\[(workspace\.)?package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{print $2; exit}' "$root/Cargo.toml")"
[ -n "$version" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }
[ -f "$binary" ] || { echo "error: binary not found: $binary" >&2; exit 1; }

dir_name="$name-$version-$target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
dir="$stage/$dir_name"
mkdir -p "$dir/examples" "$out_dir"

install -m 0755 "$binary" "$dir/$name"
case "$target" in
  *linux*)
    strip_tool="${STRIP:-strip}"
    case "$target" in
      aarch64-*) [ "$(uname -m)" = aarch64 ] || strip_tool="${STRIP:-aarch64-linux-gnu-strip}" ;;
    esac
    "$strip_tool" "$dir/$name" 2>/dev/null || true ;;
  macos*)
    # Ad-hoc signature, which Apple Silicon needs to run it.
    if command -v codesign >/dev/null 2>&1; then codesign --force --sign - "$dir/$name"; fi ;;
esac

install -m 0644 "$root/$guide" "$dir/INSTALL.md"
install -m 0644 "$root/packaging/$component/"* "$dir/examples/"
for f in LICENSE NOTICE CHANGELOG.md; do
  if [ -f "$root/$f" ]; then install -m 0644 "$root/$f" "$dir/"; fi
done

out_dir="$(cd "$out_dir" && pwd)"
tar -C "$stage" -czf "$out_dir/$dir_name.tar.gz" "$dir_name"
echo "$out_dir/$dir_name.tar.gz"
