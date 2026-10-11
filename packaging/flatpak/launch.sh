#!/bin/sh
# `flatpak run io.github.ravelund.rOsmo360` with nothing, clips or a project opens
# the player; anything else is the command line: `... export edit.json -o x.mp4`.
# The same rule as the AppImage's AppRun.
#
# The sandbox has a config folder of its own, so the colour theme wouldn't find
# KDE's colour scheme; link it in from the real one, which --filesystem=host
# shows (a link, so a later change of scheme is seen at the next start).
if [ -n "${XDG_CONFIG_HOME:-}" ] && [ -e "$HOME/.config/kdeglobals" ] && [ ! -e "$XDG_CONFIG_HOME/kdeglobals" ]; then
    ln -s "$HOME/.config/kdeglobals" "$XDG_CONFIG_HOME/kdeglobals"
fi
case "${1:-}" in
    ""|*.OSV|*.osv|*.json|*/*) exec /app/bin/rosmo360 play "$@" ;;
    *) exec /app/bin/rosmo360 "$@" ;;
esac
