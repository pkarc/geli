#!/bin/sh
# What a session added, so geli can offer to make it permanent.
#
# `snapshot` prints one `<manager> <name>` line per explicitly installed package. `report` prints
# the lines that appeared since a snapshot, each behind a marker the host greps out of the boot
# console log — the same channel the readiness marker already uses, so this needs no extra device.
#
# **Explicitly installed, not merely present.** `/etc/apk/world` holds what somebody asked for
# rather than the whole dependency closure, `--depth=0` does the same for npm, and
# `--not-required` approximates it for pip, which does not record the distinction. Without that,
# a single `apk add` would report its entire dependency tree and the prompt would be unusable.
#
# Version constraints and pins are stripped: what goes back to `.geli.json` is the name somebody
# would have written there by hand.

snapshot() {
    sed 's/[<>=~].*//' /etc/apk/world 2>/dev/null | sed 's/^/apk /'
    npm ls -g --depth=0 --parseable 2>/dev/null | sed '1d; s|.*/node_modules/||; s/^/npm /'
    pip list --not-required --format=freeze 2>/dev/null | sed 's/[=<>].*//; s/^/pip /'
}

case "$1" in
    snapshot)
        snapshot | sort -u
        ;;
    report)
        # comm needs both sides sorted, which is why `snapshot` sorts rather than the caller.
        # No baseline means nothing to compare against: report nothing rather than everything.
        [ -f "$2" ] || exit 0
        snapshot | sort -u | comm -13 "$2" - | while read -r manager name; do
            [ -n "$name" ] && echo "geli:installed $manager $name"
        done > /dev/@CONSOLE@ 2>/dev/null
        ;;
esac
