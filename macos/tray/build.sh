#!/bin/sh
set -e

cd "$(dirname "$0")"

echo "Building Tokeman Tray..."
swift build -c release 2>&1

# Create .app bundle
APP="Tokeman.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"

cp .build/release/TokemanTray "$APP/Contents/MacOS/TokemanTray"

cat > "$APP/Contents/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Tokeman</string>
    <key>CFBundleIdentifier</key>
    <string>dev.elide.tokeman.tray</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleExecutable</key>
    <string>TokemanTray</string>
    <key>LSUIElement</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
</dict>
</plist>
EOF

echo "Built: $(pwd)/$APP"
echo "Run:   open $(pwd)/$APP"
