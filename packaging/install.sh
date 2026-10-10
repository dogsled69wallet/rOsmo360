#!/bin/sh
# Installs rOsmo360 for the current user (or system-wide with --prefix /usr/local, as root):
# the binary, a menu entry and its icon.
#
# Run it from the unpacked release archive, or on its own, in which case it downloads
# the latest release first:
#   curl -fsSL https://github.com/ravelund/rOsmo360/releases/latest/download/install.sh | sh
#
# Options: --prefix DIR (default ~/.local), --uninstall.
set -eu

REPO=ravelund/rOsmo360
ARCHIVE=rosmo360-x86_64-linux.tar.gz
prefix="${HOME}/.local"
uninstall=0

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#*=}"; shift ;;
        --uninstall) uninstall=1; shift ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

bin="$prefix/bin"
apps="$prefix/share/applications"
icons="$prefix/share/icons/hicolor"

refresh() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$apps" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
}

if [ "$uninstall" = 1 ]; then
    rm -f "$bin/rosmo360" "$apps/rOsmo360.desktop" \
        "$icons/256x256/apps/rosmo360.png" "$icons/scalable/apps/rosmo360.svg"
    refresh
    echo "Removed rOsmo360 from $prefix (your settings in ~/.config/rosmo360 are kept)."
    exit 0
fi

if [ "$(uname -m)" != x86_64 ]; then
    echo "This build is for x86_64 Linux; build from source on $(uname -m)." >&2
    exit 1
fi

# Files beside this script (the unpacked archive), or download the archive.
src="$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)"
if [ ! -f "$src/rosmo360" ] || [ ! -f "$src/rOsmo360.desktop" ]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    url="https://github.com/$REPO/releases/latest/download/$ARCHIVE"
    echo "Downloading $url"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --progress-bar "$url" -o "$tmp/$ARCHIVE"
    else
        wget -q --show-progress "$url" -O "$tmp/$ARCHIVE"
    fi
    tar -xzf "$tmp/$ARCHIVE" -C "$tmp"
    src="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
fi

mkdir -p "$bin" "$apps" "$icons/256x256/apps" "$icons/scalable/apps"
install -m 755 "$src/rosmo360" "$bin/rosmo360"
install -m 644 "$src/rosmo360.png" "$icons/256x256/apps/rosmo360.png"
install -m 644 "$src/rosmo360.svg" "$icons/scalable/apps/rosmo360.svg"
# The menu entry names the binary by full path, so it works even when $bin isn't on PATH.
sed "s|^Exec=rosmo360|Exec=$bin/rosmo360|" "$src/rOsmo360.desktop" > "$apps/rOsmo360.desktop"
chmod 644 "$apps/rOsmo360.desktop"
refresh

echo "Installed $("$bin/rosmo360" --version) to $bin/rosmo360"

# Runtime needs that a binary can't bring along.
missing=""
for tool in ffmpeg ffprobe; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo
    echo "Missing:$missing. rOsmo360 decodes and encodes video through them; install ffmpeg, e.g."
    echo "  sudo apt install ffmpeg   |   sudo dnf install ffmpeg   |   sudo pacman -S ffmpeg"
fi
if ! ldconfig -p 2>/dev/null | grep -q 'libasound\.so\.2'; then
    echo
    echo "libasound (ALSA) was not found; it is needed for sound, e.g. sudo apt install libasound2."
fi
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo; echo "Note: $bin is not on your PATH; add it to run 'rosmo360' from a terminal." ;;
esac
echo
echo "Start it from the application menu (rOsmo360), or: rosmo360 play [clip.OSV | project.json]"
