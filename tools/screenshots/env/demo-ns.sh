#!/usr/bin/env bash
# Run a command in a PID namespace of its own, next to the demo services, so
# termide's process and network lists show the demo and not the recorder
# (vhs, ttyd, chromium, ffmpeg). `unshare` carries cap_sys_admin as a file
# capability in the image; the container needs --cap-add SYS_ADMIN.
# Usage: demo-ns.sh termide [args...]
# A file-capability binary runs in secure mode, which strips LD_PRELOAD from
# the environment; carry it across under another name.
export DEMO_PRELOAD="$LD_PRELOAD"
exec unshare --pid --fork --mount-proc bash -c '
    export LD_PRELOAD="$DEMO_PRELOAD"; unset DEMO_PRELOAD
    cd /tmp/demo/inventory
    /tmp/demo/bin/inventory-api -m http.server 8040 --bind 127.0.0.1 --directory data \
        >/dev/null 2>&1 &
    /tmp/demo/bin/stock-worker -m inventory.worker >/dev/null 2>&1 &
    python3 /opt/shots/mock-llm/server.py 10000 2>/dev/null &
    sleep 0.3
    exec "$@"
' demo-ns "$@"
