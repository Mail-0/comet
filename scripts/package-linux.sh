#!/usr/bin/env bash
# Package the Keiki zgx app and same-version engine for Linux.
# PROFILE=debug is an unoptimized packaging smoke check; releases use release.
# ZGX_BIN/ZGX_ENGINE may select already-built binaries; both must match VERSION.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
[[ "$PROFILE" == release || "$PROFILE" == debug ]] || { echo "PROFILE must be release or debug" >&2; exit 1; }
ARCH="$(uname -m)"
WORKSPACE="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
ZGX_VERSION="${ZGX_VERSION:-$WORKSPACE}"
[[ "$ZGX_VERSION" == "$WORKSPACE" || "$ZGX_VERSION" =~ ^"$WORKSPACE"-beta\.[0-9]+$ ]] ||
  { echo "ZGX_VERSION must be $WORKSPACE or a beta of it" >&2; exit 1; }
VERSION="$ZGX_VERSION"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/keiki-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"
mkdir -p "$OUT_DIR"
FLAGS=()
[[ "$PROFILE" != release ]] || FLAGS+=(--release)

ENGINE_BIN="${ZGX_ENGINE:-}"
if [[ -z "$ENGINE_BIN" ]]; then
  (cd "$ROOT" && ZERON_ENGINE_VERSION="$VERSION" LUAU_CXXFLAGS=-DNDEBUG \
    cargo build "${FLAGS[@]}" --locked -p zeron --bin zeron-engine)
  ENGINE_BIN="$ROOT/target/$PROFILE/zeron-engine"
elif [[ "$ENGINE_BIN" == *.tar.gz ]]; then
  UNPACK="$OUT_DIR/.stable"
  rm -rf "$UNPACK" && mkdir -p "$UNPACK"
  tar -xzf "$ENGINE_BIN" -C "$UNPACK" --strip-components=1
  ENGINE_BIN="$UNPACK/zeron-engine"
fi
[[ -x "$ENGINE_BIN" ]] || { echo "no engine at $ENGINE_BIN" >&2; exit 1; }
[[ "$("$ENGINE_BIN" --version)" == "zeron-engine $VERSION" ]] || { echo "engine version mismatch" >&2; exit 1; }

BIN="${ZGX_BIN:-}"
if [[ -z "$BIN" ]]; then
  (cd "$ROOT/apps/zgx" && ZERON_BETA_VERSION="$VERSION" cargo build "${FLAGS[@]}" --locked --features beta \
    --target-dir "$ROOT/apps/zgx/target")
  BIN="$ROOT/apps/zgx/target/$PROFILE/zeron-zgx"
fi
[[ "$("$BIN" --version)" == "zeron $VERSION" ]] || { echo "app version mismatch" >&2; exit 1; }
if [[ "$PROFILE" == release ]]; then
  ! LC_ALL=C grep -aq "Luau was built with its asserts" "$BIN" || { echo "app must use LUAU_CXXFLAGS=-DNDEBUG" >&2; exit 1; }
fi
# The background engine the app starts: sessions keep running with the app
# closed, in a process of its own.
ENGINE_BIN="${ENGINE_BIN:-$(dirname "$BIN")/zeron-engine}"
# The engine's Luau (visuals, plugin services) without its asserts: with them,
# each LUAU_ASSERT keeps its Luau source file's path (Luau/Common.h's
# __FILE__), VM/src/l*.cpp; with NDEBUG, none. (A debug package has them.)
if [[ "$PROFILE" == "release" ]] && LC_ALL=C grep -aqE 'VM[\\/]src[\\/]l[a-z]+\.cpp' "$ENGINE_BIN"; then
  echo "$ENGINE_BIN was built with Luau's asserts: build it with LUAU_CXXFLAGS=-DNDEBUG" >&2
  exit 1
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/zeron"
install -m 755 "$ENGINE_BIN" "$STAGE/zeron-engine"
install -m 644 "$ROOT/dist/zeron.desktop" "$STAGE/zeron.desktop"
install -m 644 "$ROOT/dist/zeron.png" "$STAGE/zeron.png"
install -m 644 "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" "$STAGE/"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"
cp "$ROOT/crates/voice/NOTICE.md" "$STAGE/licenses/parakeet-v3.txt"
if [[ -n "$ZGX_VERSION" ]]; then
  # The scripts and Keiki's own plugins, as scripts/package-zgx-macos.sh: read
  # beside the binary. Not zgx's generated types or the tests.
  rsync -a --exclude '.zgx/' --exclude '/tests/' --exclude '.lock' --exclude '.DS_Store' \
    "$ROOT/apps/zgx/ui/" "$STAGE/ui/"
  rsync -a --exclude '.zgx/' --exclude '.DS_Store' \
    "$ROOT/apps/zgx/plugins-builtin/" "$STAGE/plugins-builtin/"
  # Each ships the canonical SDK, as scripts/package-zgx-macos.sh.
  for dir in "$STAGE/plugins-builtin/"*/; do
    cp "$ROOT/apps/zgx/plugins-sdk/zeron.luau" "$dir/zeron.luau"
  done
  cp "$ROOT/apps/zgx/fonts/"*-OFL.txt "$STAGE/licenses/fonts/"
  rm -rf "$OUT_DIR/.stable"
fi

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Keiki for this user (no root needed), in the layout the in-app
# updater manages: ~/.keiki/app/<version> behind a `current` symlink — the
# same layout `curl -fsSL https://zeron.sh/install.sh | sh` uses — with
# ~/.local/bin/zeron and the desktop entry pointing through it.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="__VERSION__"
APP_ROOT="$HOME/.keiki/app"
# A beta's directory isn't named as a version: crates/update `managed_dir`.
case "$VERSION" in
  *-beta.*) DEST="$APP_ROOT/beta-$VERSION" ;;
  *) DEST="$APP_ROOT/$VERSION" ;;
esac
mkdir -p "$APP_ROOT"
if [ ! -x "$DEST/zeron" ]; then
  # Copy beside the final name, then rename: an interrupted install never
  # leaves a half-copied version the updater would trust.
  STAGE="$(mktemp -d "$APP_ROOT/.install-$VERSION-XXXXXX")"
  cp -R "$HERE/." "$STAGE/"
  rm -rf "$DEST"
  mv "$STAGE" "$DEST"
fi
if ! "$DEST/zeron" --version >/dev/null; then
  echo "Keiki could not start; see the loader error above. Install the missing runtime libraries (including ALSA, libasound.so.2), then retry." >&2
  exit 1
fi
ln -sfn "$DEST" "$APP_ROOT/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$APP_ROOT/current/zeron" "$HOME/.local/bin/zeron"

# Launchers list Keiki through a per-user .desktop entry. The one in the tarball
# says `Exec=zeron` and `TryExec=zeron`, which only resolve when ~/.local/bin is
# on the PATH of the desktop session (often not, e.g. a bare Wayland + fuzzel
# setup) and TryExec then hides the entry outright. So write it with absolute
# paths through the `current` symlink, which keeps working across updates. The
# icon is referenced by path too: the only artwork is 1024x1024, a size the
# hicolor theme doesn't index, so a name lookup alone can come up empty.
# (Duplicated in edge/src/install.sh, the curl installer; keep the two in sync.)
install_desktop_entry() {
  src="$1"
  app="$2"
  [ -f "$src/zeron.desktop" ] && [ -f "$src/zeron.png" ] || return 1
  case "${XDG_DATA_HOME:-}" in
    /*) data_home="$XDG_DATA_HOME" ;;
    *) data_home="$HOME/.local/share" ;;
  esac
  apps_dir="$data_home/applications"
  icon_dir="$data_home/icons/hicolor/1024x1024/apps"
  bin="$app/current/zeron"
  icon="$app/current/zeron.png"
  # Desktop Entry `Exec` quoting: double-quote an argument with reserved
  # characters, backslash-escape ", `, $ and \ inside, then double every
  # backslash again for the file's own string escaping. `%` must be `%%`.
  case "$bin" in
    *[!A-Za-z0-9_./-]*)
      exec_bin="\"$(printf '%s' "$bin" | sed -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')\""
      ;;
    *) exec_bin="$bin" ;;
  esac
  try_bin="$(printf '%s' "$bin" | sed 's/\\/\\\\/g')"
  icon_val="$(printf '%s' "$icon" | sed 's/\\/\\\\/g')"

  mkdir -p "$apps_dir" "$icon_dir" || return 1
  # Write beside the final name, then rename, so a launcher watching the
  # directory never reads a half-written entry (a leading dot is ignored).
  # Not `tmp`: sh has no `local`, and the curl installer's EXIT trap removes
  # its download dir through `$tmp`.
  entry_tmp="$apps_dir/.zeron.desktop.$$"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      Exec=*) printf 'Exec=%s %%u\n' "$exec_bin" ;;
      TryExec=*) printf 'TryExec=%s\n' "$try_bin" ;;
      Icon=*) printf 'Icon=%s\n' "$icon_val" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done <"$src/zeron.desktop" >"$entry_tmp" || { rm -f "$entry_tmp"; return 1; }
  mv -f "$entry_tmp" "$apps_dir/zeron.desktop" || { rm -f "$entry_tmp"; return 1; }
  cp "$src/zeron.png" "$icon_dir/.zeron.png.$$" \
    && mv -f "$icon_dir/.zeron.png.$$" "$icon_dir/zeron.png" || return 1

  # Best-effort cache refresh; both tools are optional. The icon cache is only
  # refreshed, never created: a user-level hicolor cache nobody else maintains
  # would hide icons other apps later install there, and the entry above
  # references the icon by path anyway.
  command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
  [ -f "$data_home/icons/hicolor/icon-theme.cache" ] \
    && command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -q -t -f "$data_home/icons/hicolor" >/dev/null 2>&1 || true
  return 0
}
install_desktop_entry "$HERE" "$APP_ROOT" \
  || echo "warn: could not install the desktop entry — Keiki won't appear in application launchers"

echo "Installed Keiki $VERSION. It updates itself from now on."
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Add ~/.local/bin to your PATH to run \`zeron\` from a terminal." ;;
esac
INSTALL
sed -i "s/__VERSION__/$VERSION/" "$STAGE/install.sh"
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
