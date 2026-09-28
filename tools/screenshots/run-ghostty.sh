#!/usr/bin/env bash
# Optional: shots that need a real terminal with the Kitty graphics protocol
# (the image viewer), which VHS's xterm.js lacks. Ghostty on the host runs the
# same isolated container; a pty driver inside plays the keys and signals each
# shot, and this script captures only that one window.
#   ./run-ghostty.sh [keys-name ...]   (default: every ghostty/*.keys)
# macOS only. Needs Screen Recording permission for the app running it.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
image=termide-screenshots
signals="$here/out/.ghostty"
helper="$here/out/.gen/window-id"

mkdir -p "$here/out/.gen" "$here/out/ghostty"
if [ ! -x "$helper" ] || [ "$here/ghostty/window-id.swift" -nt "$helper" ]; then
    # The Command Line Tools SDK, in case another SDK is active in the shell.
    env -u SDKROOT DEVELOPER_DIR=/Library/Developer/CommandLineTools \
        /Library/Developer/CommandLineTools/usr/bin/swiftc -O \
        -sdk /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
        "$here/ghostty/window-id.swift" -o "$helper"
fi

# Window titles, and so the window id, are hidden without the permission;
# check before launching anything.
if "$helper" "__permission_probe__"; [ $? -eq 2 ]; then
    cat >&2 <<'MSG'
Screen Recording permission is missing for the terminal app running this
script. Grant it in System Settings > Privacy & Security > Screen & System
Audio Recording, then quit and reopen that app and run this script again.
MSG
    exit 2
fi

[ $# -gt 0 ] || set -- $(cd "$here/ghostty" && ls *.keys | sed 's/\.keys$//')

for keys in "$@"; do
    echo "==> $keys"
    rm -rf "$signals"
    title="termide-shots-$keys-$$"
    # Ghostty's own configuration is skipped, so the frame looks the same on
    # any machine: default font and colours, a fixed grid.
    open -na Ghostty.app --args \
        --config-default-files=false --title="$title" \
        --window-width=170 --window-height=46 --font-size=13 \
        --confirm-close-surface=false --quit-after-last-window-closed=true \
        -e docker run --rm -it --network none --hostname demo \
            --tmpfs /tmp:size=2g,exec \
            -v "$here/out:/opt/shots/out" \
            -v "$here/env:/opt/shots/env:ro" \
            -v "$here/fixtures:/opt/shots/fixtures:ro" \
            -v "$here/mock-llm:/opt/shots/mock-llm:ro" \
            -v "$here/ghostty:/opt/shots/ghostty:ro" \
            --entrypoint /opt/shots/ghostty/inside.sh "$image" "$keys"

    window=""
    for _ in $(seq 60); do
        window=$("$helper" "$title" || true)
        [ -n "$window" ] && break
        sleep 0.5
    done
    [ -n "$window" ] || { echo "no window titled $title" >&2; exit 1; }

    # Wait for the driver's markers; capture the window, never the screen.
    for _ in $(seq 240); do
        for ready in "$signals"/*.ready; do
            [ -e "$ready" ] || continue
            name=$(basename "$ready" .ready)
            [ -e "$signals/$name.done" ] && continue
            screencapture -x -o -l "$window" "$here/out/ghostty/$name.png"
            touch "$signals/$name.done"
            echo "    $name.png"
        done
        # The window closes when the container exits.
        "$helper" "$title" >/dev/null || break
        sleep 0.5
    done
done
