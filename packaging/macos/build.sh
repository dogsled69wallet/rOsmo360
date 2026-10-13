#!/bin/sh
# Builds rOsmo360 from source on an Apple Silicon Mac, puts ffmpeg and
# ffprobe beside the binary (target/release/ffmpeg/), where rOsmo360 looks
# for them first, and wraps both in target/release/rOsmo360.app to open by
# double-clicking. For testing on the Mac; not a release (not notarised, so
# it runs only on the Mac that built it, or after "Open Anyway" elsewhere).
#
# Needs Rust (https://rustup.rs) and the Xcode command line tools
# (xcode-select --install). Run from anywhere: packaging/macos/build.sh
set -eu

cd "$(dirname "$0")/../.."
[ "$(uname -s)" = Darwin ] || { echo "This script is for macOS."; exit 1; }
[ "$(uname -m)" = arm64 ] || { echo "Only Apple Silicon (arm64) is supported."; exit 1; }

cargo build --release

# ffmpeg: Martin Riedl's static macOS arm64 release builds (GPL, with x264;
# signed). Downloaded once, kept unless --fresh-ffmpeg is given.
dir=target/release/ffmpeg
if [ "${1:-}" = --fresh-ffmpeg ]; then rm -rf "$dir"; fi
if [ ! -x "$dir/ffmpeg" ] || [ ! -x "$dir/ffprobe" ]; then
    mkdir -p "$dir"
    tmp=$(mktemp -d)
    for tool in ffmpeg ffprobe; do
        echo "Downloading $tool..."
        curl -fL --progress-bar -o "$tmp/$tool.zip" "https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/$tool.zip"
        unzip -o -q "$tmp/$tool.zip" -d "$dir"
        chmod +x "$dir/$tool"
    done
    rm -rf "$tmp"
fi

# What rOsmo360 depends on: VideoToolbox to decode the Osmo's HEVC on the
# GPU, x264 for MP4 export, DNxHR for the Resolve format.
ok=1
"$dir/ffmpeg" -hide_banner -hwaccels | grep -q videotoolbox || { echo "warning: this ffmpeg has no VideoToolbox: OSV playback will decode in software (slow)"; ok=0; }
"$dir/ffmpeg" -hide_banner -encoders | grep -q libx264 || { echo "warning: this ffmpeg has no libx264: MP4 export won't work"; ok=0; }
"$dir/ffmpeg" -hide_banner -encoders | grep -q dnxhd || { echo "warning: this ffmpeg has no DNxHR: the Resolve format won't work"; ok=0; }
"$dir/ffmpeg" -hide_banner -version | head -1

# The app: the binary itself (it opens the player when Finder starts it with
# nothing, `app_args`), ffmpeg and the icon.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=target/release/rOsmo360.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/rosmo360 "$app/Contents/MacOS/"
cp -R "$dir" "$app/Contents/MacOS/ffmpeg"

iconset=$(mktemp -d)/rOsmo360.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size packaging/macos/rosmo360-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) packaging/macos/rosmo360-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/rOsmo360.icns"
rm -rf "$(dirname "$iconset")"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>rOsmo360</string>
    <key>CFBundleDisplayName</key><string>rOsmo360</string>
    <key>CFBundleIdentifier</key><string>io.github.ravelund.rOsmo360</string>
    <key>CFBundleExecutable</key><string>rosmo360</string>
    <key>CFBundleIconFile</key><string>rOsmo360</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Ad hoc signature over the whole bundle, so macOS treats it as one app.
codesign --force --deep --sign - "$app" 2>/dev/null || echo "warning: couldn't sign the app ad hoc; it may still open"

echo
[ $ok = 1 ] && echo "Built $app (version $version), with ffmpeg inside." || echo "Built $app, but see the warnings above."
echo "Open it by double-clicking in Finder: open target/release"
echo "If it doesn't open, run it from here to see why: $app/Contents/MacOS/rosmo360 play"
echo "Drag it to Applications to keep it there."
echo "From the terminal: target/release/rosmo360 play [clip or project]"
