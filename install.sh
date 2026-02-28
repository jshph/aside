#!/bin/bash
set -euo pipefail

APP_NAME="aside"
BUNDLE_ID="dev.aside.app"
APP_DIR="$HOME/Applications/$APP_NAME.app"
BIN_LINK="$HOME/.local/bin/$APP_NAME"
TCC_DB="$HOME/Library/Application Support/com.apple.TCC/TCC.db"

echo "Building release binary..."
cargo build --release

echo "Creating app bundle at $APP_DIR..."
mkdir -p "$APP_DIR/Contents/MacOS"
cp target/release/$APP_NAME "$APP_DIR/Contents/MacOS/$APP_NAME"
cp bundle/Info.plist "$APP_DIR/Contents/Info.plist"

# Ad-hoc codesign with bundle ID
echo "Codesigning with identifier $BUNDLE_ID..."
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP_DIR/Contents/MacOS/$APP_NAME"

# Create CLI wrapper
mkdir -p "$(dirname "$BIN_LINK")"
cat > "$BIN_LINK" << 'EOF'
#!/bin/bash
exec "$HOME/Applications/aside.app/Contents/MacOS/aside" "$@"
EOF
chmod +x "$BIN_LINK"

# Grant audio capture permission to the terminal running aside.
# The process tap inherits TCC from the parent app (the terminal),
# so the terminal itself needs kTCCServiceAudioCapture.
echo ""
echo "Checking audio capture permissions..."
TERMINAL_ID=""
if [[ -n "${__CFBundleIdentifier:-}" ]]; then
    TERMINAL_ID="$__CFBundleIdentifier"
elif [[ "$TERM_PROGRAM" == "ghostty" ]]; then
    TERMINAL_ID="com.mitchellh.ghostty"
elif [[ "$TERM_PROGRAM" == "iTerm.app" ]]; then
    TERMINAL_ID="com.googlecode.iterm2"
elif [[ "$TERM_PROGRAM" == "Apple_Terminal" ]]; then
    TERMINAL_ID="com.apple.Terminal"
fi

if [[ -n "$TERMINAL_ID" ]]; then
    HAS_PERM=$(sqlite3 "$TCC_DB" \
        "SELECT COUNT(*) FROM access WHERE service='kTCCServiceAudioCapture' AND client='$TERMINAL_ID' AND auth_value=2;" 2>/dev/null || echo "0")
    if [[ "$HAS_PERM" == "0" ]]; then
        echo "Granting audio capture permission to $TERMINAL_ID..."
        sqlite3 "$TCC_DB" \
            "INSERT OR REPLACE INTO access (service, client, client_type, auth_value, auth_reason, auth_version, flags) VALUES ('kTCCServiceAudioCapture', '$TERMINAL_ID', 0, 2, 0, 1, 0);" 2>/dev/null \
            && echo "  Done. Restart your terminal for the permission to take effect." \
            || echo "  Failed — you may need to grant permission manually in System Settings."
    else
        echo "  $TERMINAL_ID already has audio capture permission."
    fi
else
    echo "  Could not detect terminal. If system audio is silent, run:"
    echo "    sqlite3 \"$TCC_DB\" \"INSERT OR REPLACE INTO access (service, client, client_type, auth_value, auth_reason, auth_version, flags) VALUES ('kTCCServiceAudioCapture', '<your-terminal-bundle-id>', 0, 2, 0, 1, 0);\""
fi

echo ""
echo "Installed:"
echo "  App bundle: $APP_DIR"
echo "  CLI:        $BIN_LINK"
