#!/bin/sh
# `flatpak run io.github.ravelund.rOsmo360` with nothing, clips, videos, pictures or a
# project opens the player; anything else is the command line: `... export edit.json -o x.mp4`.
# The same rule as the AppImage's AppRun.
# The types the player's Add clips dialog lists, in any case.
ext="$(printf '%s' "${1:-}" | sed -n 's/.*\.\([^./]*\)$/\1/p' | tr '[:upper:]' '[:lower:]')"
case "${1:-}:$ext" in
    :*|*/*:*|*:osv|*:json|*:mp4|*:mov|*:mkv|*:m4v|*:avi|*:webm|*:mts|*:jpg|*:jpeg|*:png|*:webp|*:bmp|*:tif|*:tiff)
        exec /app/bin/rosmo360 play "$@" ;;
    *) exec /app/bin/rosmo360 "$@" ;;
esac
