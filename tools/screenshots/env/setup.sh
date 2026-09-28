# Sourced (hidden) at the start of every tape: build the synthetic project,
# install a fresh termide configuration, start the mock model, and land in
# the project. Usage: source /opt/shots/env/setup.sh [theme]
source "$HOME/.bashrc"
# Every recording happens on the same Sunday morning: termide's clock and
# calendar, git's relative dates and file times all read this. Monotonic
# clocks stay real so timeouts and spinners behave.
export LD_PRELOAD=/usr/local/lib/libfaketime.so.1
export FAKETIME="@2026-09-27 10:42:00" FAKETIME_DONT_FAKE_MONOTONIC=1
bash /opt/shots/fixtures/make-demo.sh >/dev/null
rm -rf "$HOME/.config/termide" "$HOME/.local/share/termide" "$HOME/.cache/termide"
mkdir -p "$HOME/.config/termide"
sed "s/^theme = .*/theme = \"${1:-windows-xp}\"/" /opt/shots/env/config.toml \
    > "$HOME/.config/termide/config.toml"
if ! (exec 3<>/dev/tcp/127.0.0.1/10000) 2>/dev/null; then
    python3 /opt/shots/mock-llm/server.py 10000 2>/tmp/mock-llm.log &
    disown
    sleep 0.3
fi
cd /tmp/demo/inventory
clear
