#!/usr/bin/env bash
# Container side of run-ghostty.sh: the usual fixtures and configuration,
# then termide driven by a key script.
source /opt/shots/env/setup.sh
exec python3 /opt/shots/ghostty/pty-driver.py "/opt/shots/ghostty/$1.keys" -- termide --no-lsp
