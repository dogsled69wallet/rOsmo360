#!/bin/sh
# `flatpak run io.github.ravelund.rOsmo360` with nothing, clips or a project opens
# the player; anything else is the command line: `... export edit.json -o x.mp4`.
# The same rule as the AppImage's AppRun.
case "${1:-}" in
    ""|*.OSV|*.osv|*.json|*/*) exec /app/bin/rosmo360 play "$@" ;;
    *) exec /app/bin/rosmo360 "$@" ;;
esac
