# Packaging

## macOS

```bash
scripts/package-macos.sh
```

Produces:

- `target/package/keiki-<version>-macos-<arch>.dmg`
- `target/package/keiki-<version>-macos-<arch>-app.tar.gz`

The script builds `Keiki.app`, generates its icon set, copies bundled font
licenses, and applies an ad-hoc signature by default. Set
`CODESIGN_IDENTITY`, `NOTARY_KEY_PATH`, `NOTARY_KEY_ID`, and
`NOTARY_ISSUER_ID` for signed and notarized artifacts.

## Linux

```bash
scripts/package-linux.sh
```

Produces `target/package/keiki-<version>-linux-<arch>.tar.gz` with the binary,
desktop entry, icon, font licenses, and a user-local installer.
