#!/usr/bin/env bash
# Builds timepipe, installs it to ~/.local/bin and adds an app-launcher entry.
set -euo pipefail

cd "$(dirname "$0")"
cargo build --release

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
mkdir -p "$BIN_DIR" "$APP_DIR"

install -m 755 target/release/timepipe "$BIN_DIR/timepipe"

cat > "$APP_DIR/timepipe.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=timepipe
GenericName=Time Tracker
Comment=Terminal time tracker
Exec=$BIN_DIR/timepipe
Icon=appointment-soon
Terminal=true
Categories=Utility;Office;
Keywords=time;tracker;timer;
EOF

command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" || true

echo "Installed: $BIN_DIR/timepipe"
echo "Launcher:  $APP_DIR/timepipe.desktop (search for 'timepipe' in your app menu)"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: add $BIN_DIR to your PATH to run 'timepipe' from a terminal." ;;
esac
