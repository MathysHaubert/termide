# Sourced between shots of the theme gallery: switch the configured theme and
# forget the saved layout, so the next launch starts from the same state.
# Usage: source /opt/shots/env/theme.sh <theme-id>
sed -i "s/^theme = .*/theme = \"$1\"/" "$HOME/.config/termide/config.toml"
rm -rf "$HOME/.local/share/termide" "$HOME/.cache/termide"
clear
