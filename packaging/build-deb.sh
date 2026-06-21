#!/bin/sh
# Build a .deb from the release binary.  Usage: packaging/build-deb.sh [version]
# Requires: a built release binary (cargo build --release) and dpkg-deb.
set -e
VER="${1:-0.1.0}"
ARCH="${ARCH:-amd64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/sdirstat"
[ -x "$BIN" ] || { echo "build the release binary first:  cargo build --release" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/applications" \
         "$STAGE/usr/share/icons/hicolor/scalable/apps" "$STAGE/usr/share/doc/sdirstat"

install -m755 "$BIN"                          "$STAGE/usr/bin/sdirstat"
install -m755 "$ROOT/packaging/sdirstat-gui"  "$STAGE/usr/bin/sdirstat-gui"
install -m644 "$ROOT/packaging/sdirstat.desktop" "$STAGE/usr/share/applications/sdirstat.desktop"
install -m644 "$ROOT/packaging/sdirstat.svg"  "$STAGE/usr/share/icons/hicolor/scalable/apps/sdirstat.svg"
install -m644 "$ROOT/README.md"               "$STAGE/usr/share/doc/sdirstat/README.md"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: sdirstat
Version: $VER
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: Clay Townsend <clay@twn.systems>
Depends: libc6
Recommends: libglib2.0-bin, xdg-utils
Description: Parallel disk-usage analyzer with a treemap/sunburst web GUI
 sdirstat scans a directory tree in parallel and reports allocated sizes
 byte-exact with du.  It serves an interactive web GUI (treemap, sunburst,
 file-type statistics, Open/Reveal/Trash actions) and also emits the QDirStat
 cache format, a JSON tree, or a self-contained HTML report.  Zero runtime
 dependencies.
EOF

OUT="$ROOT/dist"; mkdir -p "$OUT"
DEB="$OUT/sdirstat_${VER}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB"
echo "built $DEB"
