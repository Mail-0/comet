# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces three Linux artifacts (narrow with `FORMATS="tarball deb appimage"`):

- `target/package/zeron-<version>-linux-<arch>.tar.gz`, containing:
  - `zeron` — the binary (headed by default; `zeron headless` runs the engine alone)
  - `zeron.desktop` — XDG desktop entry
  - `zeron.png` — 1024×1024 app icon (rendered from `dist/zeron.svg`)
  - `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`
- `target/package/zeron-<version>-linux-<debarch>.deb` — a Debian package
  installing the binary, desktop entry, icon, and font licenses under `/usr`.
  Its `Depends` come from `dpkg-shlibdeps` plus the two libraries gpui
  `dlopen`s (`libvulkan1`, `libwayland-client0`), which shlibdeps cannot see.
- `target/package/zeron-<version>-linux-<arch>.AppImage` — a self-contained
  AppImage for systems without a package manager.

The `.deb` and AppImage payloads are stripped; the tarball's is not.

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/zeron-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Zeron.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
 dmg. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p zeron --target aarch64-apple-darwin
   cargo build --release -p zeron --target x86_64-apple-darwin
   lipo -create -output zeron \
     target/aarch64-apple-darwin/release/zeron \
     target/x86_64-apple-darwin/release/zeron
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Zeron.app/Contents/{MacOS,Resources}
   cp zeron Zeron.app/Contents/MacOS/zeron
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Zeron.app/Contents/Info.plist
   ```
3. Icon: generate `zeron.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Zeron.app/Contents/Resources/zeron.icns`:
   ```sh
   mkdir zeron.iconset && sips -z 256 256 dist/macos/icon-1024.png --out zeron.iconset/icon_256x256.png
   iconutil -c icns zeron.iconset -o Zeron.app/Contents/Resources/zeron.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Zeron.app
   xcrun notarytool submit Zeron.zip --keychain-profile … --wait
   xcrun stapler staple Zeron.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Zeron -srcfolder Zeron.app -ov -format UDZO Zeron.dmg`).

## Icon artwork

`dist/zeron.svg` is the master: the Keiki rocking-horse mark in white on the
near-black `#04060B` tile with a thin `#0091FF` rim. Both PNGs are rendered
from it and committed, so packaging needs no SVG toolchain:

```sh
# Linux — full-bleed 1024 tile, corners already transparent in the SVG
python3 -c "import cairosvg; cairosvg.svg2png(url='dist/zeron.svg', write_to='dist/zeron.png', output_width=1024, output_height=1024)"

# macOS — Big Sur grid: 824 body (corner radius widened to Apple's ratio),
# centred on a 1024 canvas with the drop shadow baked in
python3 -c "import cairosvg,pathlib; s=pathlib.Path('dist/zeron.svg').read_text().replace('rx=\"180\"','rx=\"230\"').replace('rx=\"176\"','rx=\"226\"'); cairosvg.svg2png(bytestring=s.encode(), write_to='body.png', output_width=824, output_height=824)"
convert body.png \( +clone -background black -shadow 45x18+0+14 \) +swap \
  -background none -layers merge +repage body-shadow.png
convert -size 1024x1024 xc:none body-shadow.png -gravity center -geometry +0+6 \
  -composite dist/macos/icon-1024.png
```

`crates/ui/assets/icons/zeron-logo.svg` is the same mark as a bare
`currentColor` silhouette, used for the in-app watermark.
