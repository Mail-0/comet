# Keiki

Keiki is a Keiki-only desktop workspace built on Zeron's Rust engine and the
ZGX native desktop framework. The interface is Luau, not the former GPUI app.
Keiki Copilot is the production harness; Mock remains for tests.

## Development

```sh
cargo check --workspace
cargo check --manifest-path apps/zgx/Cargo.toml
cd apps/zgx
zgx test ui
zgx lint ui
zgx check ui
```

The root workspace supplies the engine and CLI (`apps/zeron`); `apps/zgx` is a
separate Cargo workspace with its own lockfile. Run app builds from its directory
so its Cargo configuration enables Luau's NDEBUG. Rust crates and executables
retain internal `zeron` names for compatibility. Strict `zgx check ui` is a
separate manual check, not part of the Luau test workflow; inherited strict
SDK/template diagnostics remain. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Account and data

Keiki OAuth defaults to `https://onkeiki.com` (`KEIKI_API_URL` overrides it).
The engine owns access credentials and stores refresh credentials in the OS
keyring, using the existing Comet entries. Luau receives display data and opaque
local handles, never access/refresh tokens or capability-bearing URLs.

Local is the only active profile. Data and default worktrees live in `~/.keiki`;
signing in or changing organizations does not switch databases. Archived
account-scoped stores and uploads remain untouched, not migrated or merged.
Legacy local-only `~/.zeron` / `~/.comet-native` data can be discovered on first
launch; account archives are excluded. `ZERON_DATA_DIR` and
`ZERON_WORKTREES_DIR` remain supported overrides.

## Packages and updates

Linux and macOS releases build the ZGX workspace and produce `keiki-*` assets:

```sh
scripts/package-linux.sh
# macOS, with the same-version engine already built:
ZGX_VERSION=0.3.9 ZGX_ENGINE=target/release/zeron-engine scripts/package-zgx-macos.sh
```

Linux's tarball includes an `install.sh` for `~/.keiki/app`; macOS ships
`Keiki.app` in a DMG and an app tarball for transactional updates. `PROFILE=debug`
on the Linux script is an unoptimized local packaging smoke check, not a release.

The updater defaults to Comet's GitHub Releases API:
`https://api.github.com/repos/Mail-0/comet/releases/latest`.
It reads the release tag and asset API URLs, plus `manifest.json` checksums and
platform requirements when present. `KEIKI_RELEASES_URL` overrides the full
GitHub release API URL or a custom base serving `manifest.json` and its artifacts
(legacy `latest.txt` is supported). Private GitHub releases can use
`KEIKI_UPDATE_TOKEN` or `GITHUB_TOKEN`; these are sent only to the HTTPS GitHub
API, never to arbitrary download hosts. There is no Zeron edge/R2 release fallback.

A version bump on main creates `v<workspace version>` and explicitly dispatches
the release workflow; pushing a tag with GitHub's token alone cannot trigger it.
Stable releases publish Linux x86_64/aarch64 and macOS arm64 assets and a checksum
manifest on GitHub. `beta-v<version>-beta.<n>` publishes a GitHub prerelease;
opted-in beta clients also receive stable releases. Existing bundle identifiers,
internal executable names and OAuth keyring entries are preserved.

**Release migration warning:** the former GPUI bundle supported macOS 12;
ZGX requires macOS 13. Its old updater does not read the new minimum-OS
manifest, so publishing the same DMG name as GitHub's latest release can offer
an unsupported upgrade to a GPUI installation on macOS 12. Decide that migration
policy before publishing the first ZGX release. The new ZGX updater itself
honors the manifest's minimum macOS version.

## Deferred

- Desktop (VNC) and Browser (JPEG-frame) viewers say “view coming soon”; no
  capability request, viewer transport or token-bearing URL is exposed to Luau.
- The deep sync / DeviceRoom / chat2 transport purge is deferred. Retained
  transport code is inert in the Local-only product; it has no account token.

Derived from Zeron under the [MIT license](LICENSE).
