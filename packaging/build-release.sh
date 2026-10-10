#!/bin/sh
# Builds the release assets into dist/: the binary archive (with install.sh), the
# AppImage, install.sh on its own and SHA256SUMS. Needs docker and network access.
set -eu
cd "$(dirname "$0")/.."
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
dist=dist
rm -rf "$dist" && mkdir -p "$dist"

docker build -q -t rosmo360-build:jammy packaging >/dev/null
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/src -w /src \
    -e CARGO_TARGET_DIR=/src/target/jammy rosmo360-build:jammy cargo build --release --locked
exe=target/jammy/release/rosmo360
strip "$exe"

# Archive: rosmo360-x86_64-linux/ with the binary, install script, menu entry and icons.
# The asset names carry no version, so .../releases/latest/download/<name> stays valid.
pkg="$dist/rosmo360-x86_64-linux"
mkdir -p "$pkg"
cp "$exe" packaging/install.sh packaging/rOsmo360.desktop packaging/rosmo360.png \
    packaging/rosmo360.svg README.md LICENSE-MIT LICENSE-APACHE "$pkg/"
chmod 755 "$pkg/rosmo360" "$pkg/install.sh"
tar -czf "$dist/rosmo360-x86_64-linux.tar.gz" -C "$dist" rosmo360-x86_64-linux
cp packaging/install.sh "$dist/install.sh"

# AppImage. ffmpeg and ALSA come from the system: a bundled ffmpeg would lose the
# GPU decoders (VA-API, Vulkan, CUDA) that full-res stills and export depend on.
app="$dist/rOsmo360.AppDir"
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/icons/hicolor/256x256/apps"
cp "$exe" "$app/usr/bin/"
cp packaging/AppRun "$app/AppRun" && chmod 755 "$app/AppRun"
cp packaging/rOsmo360.desktop "$app/" && cp packaging/rOsmo360.desktop "$app/usr/share/applications/"
cp packaging/rosmo360.png "$app/" && cp packaging/rosmo360.png "$app/usr/share/icons/hicolor/256x256/apps/"
ln -s rosmo360.png "$app/.DirIcon"
tool=target/appimagetool-x86_64.AppImage
if [ ! -x "$tool" ]; then
    curl -fsSL https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage -o "$tool"
    chmod 755 "$tool"
fi
ARCH=x86_64 VERSION="$version" "$tool" --appimage-extract-and-run -n "$app" "$dist/rOsmo360-x86_64.AppImage" >/dev/null
rm -rf "$app" "$pkg"

(cd "$dist" && sha256sum rosmo360-x86_64-linux.tar.gz rOsmo360-x86_64.AppImage install.sh > SHA256SUMS)
ls -l "$dist"
