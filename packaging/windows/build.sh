#!/bin/sh
# Builds the Windows version on Linux: rosmo360.exe cross-compiled in docker
# (MinGW), with ffmpeg and ffprobe in an ffmpeg\ folder beside it, where
# rOsmo360 looks first (`ffmpeg::tool`), zipped as
# target/windows/rOsmo360-x86_64-windows.zip. For testing; not a release yet
# (no console hiding, no icon, unsigned).
#
# Needs docker, curl, unzip, python3 and network access; wine (optional) checks
# the ffmpeg. Run from anywhere: packaging/windows/build.sh [--fresh-ffmpeg]
set -eu
cd "$(dirname "$0")/../.."

# The icon (rosmo360.ico, rendered from packaging/rosmo360.svg at 16-256 px)
# goes into the .exe as a resource, compiled by MinGW's windres and linked
# in, so Explorer, the desktop and shortcuts show it.
mkdir -p target/windows
printf '1 ICON "packaging/windows/rosmo360.ico"\n' > target/windows/icon.rc
docker build -q -t rosmo360-build:windows packaging/windows >/dev/null
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/src -w /src \
    -e CARGO_TARGET_DIR=/src/target/windows -e RUSTFLAGS="-C link-arg=/src/target/windows/icon.o" rosmo360-build:windows \
    sh -c 'x86_64-w64-mingw32-windres target/windows/icon.rc -O coff -o target/windows/icon.o &&
           cargo build --release --locked --target x86_64-pc-windows-gnu &&
           x86_64-w64-mingw32-strip target/windows/x86_64-pc-windows-gnu/release/rosmo360.exe'
exe=target/windows/x86_64-pc-windows-gnu/release/rosmo360.exe

# ffmpeg: BtbN's GPL builds of the 8.1 release branch (x264, DNxHR, the
# d3d11va, CUDA and Vulkan decoders), the shared variant: ffmpeg.exe and
# ffprobe.exe share their DLLs, half the size of two static programs.
# Checked against BtbN's checksums; kept unless --fresh-ffmpeg is given.
ff=target/windows/ffmpeg
name=ffmpeg-n8.1-latest-win64-gpl-shared-8.1
if [ "${1:-}" = --fresh-ffmpeg ]; then rm -rf "$ff"; fi
if [ ! -f "$ff/ffmpeg.exe" ] || [ ! -f "$ff/ffprobe.exe" ]; then
    tmp=$(mktemp -d)
    base=https://github.com/BtbN/FFmpeg-Builds/releases/download/latest
    echo "Downloading $name..."
    curl -fL --progress-bar -o "$tmp/$name.zip" "$base/$name.zip"
    curl -fsSL -o "$tmp/checksums.sha256" "$base/checksums.sha256"
    (cd "$tmp" && grep " $name.zip\$" checksums.sha256 | sha256sum -c --quiet -)
    unzip -q "$tmp/$name.zip" -d "$tmp"
    rm -rf "$ff" && mkdir -p "$ff"
    cp "$tmp/$name"/bin/ffmpeg.exe "$tmp/$name"/bin/ffprobe.exe "$tmp/$name"/bin/*.dll "$ff/"
    cp "$tmp/$name/LICENSE.txt" "$ff/"
    rm -rf "$tmp"
fi

# What rOsmo360 depends on: a Windows GPU decoder for the Osmo's HEVC, x264
# for MP4 export, DNxHR for the Resolve format.
if command -v wine >/dev/null; then
    run() { WINEDEBUG=-all wine "$ff/ffmpeg.exe" -hide_banner "$@" 2>/dev/null | tr -d '\r'; }
    run -hwaccels | grep -q d3d11va || echo "warning: this ffmpeg has no d3d11va"
    run -encoders | grep -q libx264 || echo "warning: this ffmpeg has no libx264: MP4 export won't work"
    run -encoders | grep -q dnxhd || echo "warning: this ffmpeg has no DNxHR: the Resolve format won't work"
    run -version | head -1
fi

# The folder to zip: the program, ffmpeg\, the licences (ours, the fonts'
# and ffmpeg's) and where ffmpeg's source is.
out=target/windows/rOsmo360
rm -rf "$out" && mkdir -p "$out/fonts"
cp "$exe" README.md LICENSE-MIT LICENSE-APACHE "$out/"
cp -r "$ff" "$out/ffmpeg"
for d in fonts/*/; do mkdir -p "$out/fonts/$(basename "$d")" && cp "$d"*.txt "$out/fonts/$(basename "$d")/"; done
cp fonts/README.md "$out/fonts/"
cat > "$out/ffmpeg/SOURCE.txt" <<EOF
ffmpeg.exe, ffprobe.exe and the DLLs beside them are BtbN's GPL build
$name of FFmpeg (https://ffmpeg.org), licensed under the GNU GPL (LICENSE.txt).
rOsmo360 runs them as separate programs. The build's sources and scripts:
https://github.com/BtbN/FFmpeg-Builds
EOF
rm -f target/windows/rOsmo360-x86_64-windows.zip
(cd target/windows && python3 -m zipfile -c rOsmo360-x86_64-windows.zip rOsmo360)
ls -l target/windows/rOsmo360-x86_64-windows.zip
