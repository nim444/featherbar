#!/bin/sh
# Assemble Featherbar.app from the release binary.
# SMAppService (the launch-at-login toggle) only works from a real .app bundle.
set -eu

cd "$(dirname "$0")/.."

cargo build --release

APP=target/Featherbar.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"

cp target/release/featherbar "$APP/Contents/MacOS/featherbar"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.nim444.featherbar</string>
    <key>CFBundleName</key>
    <string>Featherbar</string>
    <key>CFBundleDisplayName</key>
    <string>Featherbar</string>
    <key>CFBundleExecutable</key>
    <string>featherbar</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# Ad-hoc signature so macOS will run it locally without a developer cert.
codesign --force --sign - "$APP"

echo "Built $APP"
echo "Install: cp -R $APP /Applications/   (login item registration is most reliable from /Applications)"
