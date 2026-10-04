#!/usr/bin/env bash
# Packages target/release/sorrel for this OS into dist/. Signs when the
# signing secrets are present in the environment; otherwise ships unsigned.
#
#   Windows: WINDOWS_CERT_PFX (base64), WINDOWS_CERT_PASSWORD
#   macOS:   APPLE_CERT_P12 (base64), APPLE_CERT_PASSWORD, APPLE_SIGNING_IDENTITY,
#            APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD (notarization)
set -euo pipefail

version="${1:?usage: package.sh VERSION}"
mkdir -p dist
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    # One-click installer (scripts/sorrel.iss). Needs Inno Setup 6:
    # `winget install JRSoftware.InnoSetup` locally, choco in CI.
    setup="dist/sorrel-$version-windows-x64-setup.exe"
    sign() {
      [ -n "${WINDOWS_CERT_PFX:-}" ] || return 0
      echo "$WINDOWS_CERT_PFX" | base64 -d > cert.pfx
      signtool=$(ls "/c/Program Files (x86)/Windows Kits/10/bin/"*/x64/signtool.exe | tail -1)
      MSYS2_ARG_CONV_EXCL='*' "$signtool" sign /f cert.pfx /p "$WINDOWS_CERT_PASSWORD" /fd sha256 /tr http://timestamp.digicert.com /td sha256 "$1"
      rm cert.pfx
    }
    sign target/release/sorrel.exe
    for iscc in "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" "${LOCALAPPDATA:-}/Programs/Inno Setup 6/ISCC.exe"; do
      [ -f "$iscc" ] && break
    done
    "$iscc" -Q "-DVersion=$version" scripts/sorrel.iss
    sign "$setup"
    ;;
  Darwin)
    app=dist/Sorrel.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp target/release/sorrel "$app/Contents/MacOS/sorrel"
    cp crates/app/icon/sorrel.icns "$app/Contents/Resources/sorrel.icns"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Sorrel</string>
  <key>CFBundleIdentifier</key><string>app.sorrel.desktop</string>
  <key>CFBundleExecutable</key><string>sorrel</string>
  <key>CFBundleIconFile</key><string>sorrel</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
    if [ -n "${APPLE_CERT_P12:-}" ]; then
      keychain=build.keychain
      security create-keychain -p ci "$keychain"
      security default-keychain -s "$keychain"
      security unlock-keychain -p ci "$keychain"
      echo "$APPLE_CERT_P12" | base64 -d > cert.p12
      security import cert.p12 -k "$keychain" -P "$APPLE_CERT_PASSWORD" -T /usr/bin/codesign
      security set-key-partition-list -S apple-tool:,apple: -s -k ci "$keychain"
      rm cert.p12
      codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$app"
    fi
    zip="dist/sorrel-$version-macos-$(uname -m).zip"
    ditto -c -k --keepParent "$app" "$zip"
    if [ -n "${APPLE_ID:-}" ]; then
      xcrun notarytool submit "$zip" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" --wait
      xcrun stapler staple "$app"
      ditto -c -k --keepParent "$app" "$zip"
    fi
    rm -rf "$app"
    ;;
  Linux)
    stage="dist/sorrel-$version-linux-x64"
    mkdir -p "$stage"
    cp target/release/sorrel "$stage/"
    cp crates/app/icon/sorrel.png "$stage/sorrel.png"
    cat > "$stage/sorrel.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Sorrel
Exec=sorrel
Icon=sorrel
Categories=Development;Utility;
DESKTOP
    tar -C dist -czf "$stage.tar.gz" "$(basename "$stage")"
    rm -rf "$stage"
    ;;
esac
ls -l dist
