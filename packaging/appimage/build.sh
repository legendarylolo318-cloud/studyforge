#!/bin/sh
# Build StudyForge-x86_64.AppImage from target/release (run `cargo build --release` first).
# Bundles studyforge + studyd and the tray/GTK libraries; needs network for linuxdeploy.
set -eu
cd "$(dirname "$0")/../.."
OUT=${OUT:-dist}
mkdir -p "$OUT" build/appdir && rm -rf build/appdir/*
for t in linuxdeploy-x86_64.AppImage; do
    [ -x "build/$t" ] || { curl -sSfL -o "build/$t" "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/$t"; chmod +x "build/$t"; }
done
install -Dm755 target/release/studyd build/appdir/usr/bin/studyd
export APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 LDAI_OUTPUT="$OUT/StudyForge-x86_64.AppImage"
EXTRA=""
for lib in libayatana-appindicator3.so.1 libappindicator3.so.1; do
    p=$(ldconfig -p | awk -v l="$lib" '$1==l {print $NF; exit}')
    [ -n "$p" ] && EXTRA="$EXTRA --library $p"
done
# shellcheck disable=SC2086
./build/linuxdeploy-x86_64.AppImage --appdir build/appdir \
    --executable target/release/studyforge \
    --desktop-file packaging/linux/studyforge.desktop \
    --icon-file packaging/linux/studyforge.png \
    $EXTRA --output appimage
ls -la "$OUT"
