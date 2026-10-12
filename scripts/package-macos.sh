#!/usr/bin/env bash
# Package a macOS build as Crusteo.app inside crusteo-<version>-macos-<arch>.zip.
#
#   scripts/package-macos.sh [path/to/crusteo] [out-dir]
#
# Defaults: target/release/crusteo and dist/. Pass a lipo'd universal binary
# to get a universal app. The bundle is ad-hoc signed only (not notarized);
# users clear the quarantine flag with `xattr -dr com.apple.quarantine`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${1:-$root/target/release/crusteo}"
out_dir="${2:-$root/dist}"

version="$(awk -F'"' '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{print $2; exit}' "$root/Cargo.toml")"
[ -n "$version" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }
[ -f "$binary" ] || { echo "error: binary not found: $binary" >&2; exit 1; }
# macOS wants a numeric bundle version: 0.1.0-alpha.1 is 0.1.0 there; the zip keeps the full one.
bundle_version="${version%%-*}"

archs="$(lipo -archs "$binary" 2>/dev/null || uname -m)"
case "$archs" in
  *" "*) arch=universal ;;
  *) arch="$archs" ;;
esac

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
app="$stage/Crusteo.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out_dir"

cp "$binary" "$app/Contents/MacOS/crusteo"
chmod 0755 "$app/Contents/MacOS/crusteo"
# The Dock and Cmd-Tab icon, from the 1024px PNG.
iconset="$stage/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$root/assets/icon/crusteo.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$root/assets/icon/crusteo.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

for f in README.md LICENSE NOTICE; do
  if [ -f "$root/$f" ]; then cp "$root/$f" "$app/Contents/Resources/"; fi
done

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Crusteo</string>
  <key>CFBundleExecutable</key>
  <string>crusteo</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>dev.chicagobuss.crusteo</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Crusteo</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$bundle_version</string>
  <key>CFBundleVersion</key>
  <string>$bundle_version</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key>
  <string>10.15.7</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Crusteo connects to Paseo daemons and the services you configure on your local network.</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key>
  <true/>
</dict>
</plist>
EOF
if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$app/Contents/Info.plist"
fi

# Ad-hoc signature so Apple Silicon will run the modified bundle.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$app"
fi

out_dir="$(cd "$out_dir" && pwd)"
zip_path="$out_dir/crusteo-$version-macos-$arch.zip"
rm -f "$zip_path"
if command -v ditto >/dev/null 2>&1; then
  ditto -c -k --sequesterRsrc --keepParent "$app" "$zip_path"
else
  (cd "$stage" && zip -qry "$zip_path" Crusteo.app)
fi
echo "$zip_path"
