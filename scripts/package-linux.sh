#!/usr/bin/env bash
# Linux packaging: build the release binary and produce a tarball, Debian
# package, and AppImage under target/package. The tarball contains the binary,
# the .desktop entry, and the icon, plus an install.sh that drops them into
# ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.
#        FORMATS="tarball deb appimage" (default: all three).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
FORMATS="${FORMATS:-tarball deb appimage}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/zeron-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"
case "$ARCH" in
  x86_64) DEB_ARCH="amd64" ;;
  aarch64) DEB_ARCH="arm64" ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

has_format() {
  local format
  for format in $FORMATS; do
    [[ "$format" == "$1" ]] && return 0
  done
  return 1
}

if ! has_format tarball && ! has_format deb && ! has_format appimage; then
  echo "FORMATS must include tarball, deb, or appimage" >&2
  exit 1
fi

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p zeron
  BIN="$ROOT/target/release/zeron"
else
  cargo build -p zeron
  BIN="$ROOT/target/debug/zeron"
fi

rm -rf "$STAGE" "$TARBALL" "$OUT_DIR/zeron-$VERSION-linux-$DEB_ARCH.deb" \
  "$OUT_DIR/zeron-$VERSION-linux-$ARCH.AppImage" "$OUT_DIR/.staging"
mkdir -p "$OUT_DIR/.staging"
trap 'rm -rf "$STAGE" "$OUT_DIR/.staging"' EXIT

if has_format tarball; then
  mkdir -p "$STAGE"
  install -m 755 "$BIN" "$STAGE/zeron"
  install -m 644 "$ROOT/dist/zeron.desktop" "$STAGE/zeron.desktop"
  install -m 644 "$ROOT/dist/zeron.png" "$STAGE/zeron.png"
  mkdir -p "$STAGE/licenses/fonts"
  cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"

  cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Zeron into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/zeron" "$HOME/.local/bin/zeron"
install -Dm644 "$HERE/zeron.desktop" "$HOME/.local/share/applications/zeron.desktop"
install -Dm644 "$HERE/zeron.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/zeron.png"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Installed. Make sure ~/.local/bin is on your PATH."
INSTALL
  chmod 755 "$STAGE/install.sh"

  tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
  echo "packaged: $TARBALL"
  tar -tzf "$TARBALL"
fi

if has_format deb || has_format appimage; then
  STRIPPED_BIN="$OUT_DIR/.staging/zeron"
  install -m 755 "$BIN" "$STRIPPED_BIN"
  strip -s "$STRIPPED_BIN"
fi

if has_format deb; then
  DEB_ROOT="$OUT_DIR/.staging/deb"
  mkdir -p "$DEB_ROOT/DEBIAN" \
    "$DEB_ROOT/usr/bin" \
    "$DEB_ROOT/usr/share/applications" \
    "$DEB_ROOT/usr/share/icons/hicolor/1024x1024/apps" \
    "$DEB_ROOT/usr/share/doc/zeron/licenses/fonts"
  install -m 755 "$STRIPPED_BIN" "$DEB_ROOT/usr/bin/zeron"
  install -m 644 "$ROOT/dist/zeron.desktop" "$DEB_ROOT/usr/share/applications/zeron.desktop"
  install -m 644 "$ROOT/dist/zeron.png" \
    "$DEB_ROOT/usr/share/icons/hicolor/1024x1024/apps/zeron.png"
  cp "$ROOT/crates/ui/assets/fonts/licenses/"* \
    "$DEB_ROOT/usr/share/doc/zeron/licenses/fonts/"

  mkdir -p "$DEB_ROOT/debian"
  cat >"$DEB_ROOT/debian/control" <<'CONTROL'
Source: zeron
Section: devel
Priority: optional
Maintainer: package builder <noreply@onkeiki.com>
Package: zeron
Architecture: all
Description: Zeron desktop agent client
 Desktop client for controlling Keiki coding agents.
CONTROL
  DEPS="$(
    cd "$DEB_ROOT"
    dpkg-shlibdeps -O --ignore-missing-info ./usr/bin/zeron
  )"
  DEPS="${DEPS#shlibs:Depends=}, libvulkan1, libwayland-client0"
  cat >"$DEB_ROOT/DEBIAN/control" <<CONTROL
Package: zeron
Version: $VERSION
Architecture: $DEB_ARCH
Section: devel
Priority: optional
Maintainer: ${DEB_MAINTAINER:-Mail-0 <noreply@onkeiki.com>}
Depends: $DEPS
Recommends: mesa-vulkan-drivers
Description: Zeron desktop agent client
 Desktop client for controlling Keiki coding agents.
CONTROL
  rm -rf "$DEB_ROOT/debian"
  dpkg-deb --root-owner-group --build "$DEB_ROOT" \
    "$OUT_DIR/zeron-$VERSION-linux-$DEB_ARCH.deb" >/dev/null
  echo "packaged: $OUT_DIR/zeron-$VERSION-linux-$DEB_ARCH.deb"
fi

if has_format appimage; then
  APPDIR="$OUT_DIR/.staging/zeron.AppDir"
  mkdir -p "$APPDIR/usr/bin" \
    "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/1024x1024/apps"
  install -m 755 "$STRIPPED_BIN" "$APPDIR/usr/bin/zeron"
  install -m 644 "$ROOT/dist/zeron.desktop" "$APPDIR/usr/share/applications/zeron.desktop"
  install -m 644 "$ROOT/dist/zeron.desktop" "$APPDIR/zeron.desktop"
  install -m 644 "$ROOT/dist/zeron.png" \
    "$APPDIR/usr/share/icons/hicolor/1024x1024/apps/zeron.png"
  install -m 644 "$ROOT/dist/zeron.png" "$APPDIR/zeron.png"
  ln "$APPDIR/zeron.png" "$APPDIR/.DirIcon"
  cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
set -euo pipefail
APPDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$APPDIR/usr/bin/zeron" "$@"
APPRUN
  chmod 755 "$APPDIR/AppRun"

  TOOLS="$OUT_DIR/.tools"
  mkdir -p "$TOOLS"
  TOOL="$TOOLS/appimagetool-$ARCH.AppImage"
  if [[ ! -x "$TOOL" ]]; then
    curl -fL --retry 3 \
      "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$ARCH.AppImage" \
      -o "$TOOL"
    chmod 755 "$TOOL"
  fi
  APPIMAGE="$OUT_DIR/zeron-$VERSION-linux-$ARCH.AppImage"
  APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$TOOL" "$APPDIR" "$APPIMAGE"
  echo "packaged: $APPIMAGE"
fi
