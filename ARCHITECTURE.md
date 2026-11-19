# Keiki architecture

Keiki is a Keiki-only product on a current-Zeron-derived Rust engine and ZGX
native desktop UI. The former GPUI frontend, WorkOS account flow and Zeron edge
services are not the shipped application or its account/update backends.

## Runtime boundary

```text
ZGX Luau UI → native Rust bridge → engine RPC / watch streams
                                      ├─ Local SQLite / Loro docs (~/.keiki)
                                      ├─ Keiki Auth → keiki-api → onkeiki.com
                                      └─ Copilot: bound agent conversation or dashboard thread
```

`apps/zgx` is an independent Cargo workspace. Its Rust host mounts the existing
ZGX shell, transcript/composer, dialogs, avatar and terminal components; Luau
owns presentation, not credentials. The root workspace builds the engine and CLI
from `apps/zeron`. Internal binary names remain `zeron`, `zeron-engine` and
`zeron-zgx` so managed installs, command delegation and existing bundles work.
The macOS bundle is `Keiki.app`; the inherited bundle identifier is retained.

## Authentication and storage

The engine's Auth facade performs Keiki OAuth/session refresh and organization
selection through `keiki-api`. It validates an email and uses its trimmed,
lowercase value as the facade user ID. Refresh credentials and client/token
metadata use the OS keyring with Comet's existing service and entry names; access
tokens stay in Rust. Missing identity or revoked refresh credentials signs out.

Local is the only active workspace under `~/.keiki`; authentication and org
changes never select Synced/Development stores. Historical account stores and
uploads are left untouched on disk, not imported, merged or deleted. Local-only
legacy-directory discovery excludes those archives. Existing path overrides and
Unix legacy-path compatibility are retained for Git worktrees.

## Keiki browsing and conversations

The engine lists agents/groups as whole lists, matching the existing API. Agent
conversations and history are paginated. A persistent binding is keyed under the
normalized account by org + agent + conversation ID. Listing creates no chats;
opening resolves a stable local chat ID and imports history without replaying it
as prompts. New drafts obtain a remote conversation on first send.

The sidebar polls at 15 seconds after success / 10 after failure; an open
transcript polls at 10 seconds idle / 3 live. Scope revisions discard late replies
on account/org changes. Bound conversations use Keiki's existing send/streamed
steer path and reconcile remote history; dashboard-thread Copilot mode remains
separate. Stop drops the local stream and reconciles; it does not invent a remote
cancellation endpoint. Keiki Copilot is the only production harness; Mock remains
for tests.

Existing ZGX chat components render imported history, streaming output and tools.
Pending questions are read-only cards above the composer with a composer/steer
fallback, since agent conversations have no answer/resume API contract. Resolved
questions become transcript chips. Local chats retain their own sidebar section.

## Actions and sandbox surfaces

Scoped engine RPCs own local rename/archive/delete/pin metadata, existing remote
block/takeover/peer-end actions, group rosters/end-work, task cancellation,
templates/creation and MCP authorization. No invented swarm-assignment/start API
is exposed. Group membership is navigation, not a second conversation identity.
Private dashboard/MCP URLs are opened by the engine rather than returned to Luau.

Agent avatars are fetched and bounded in Rust; the bridge materializes a maximum
of 64 temporary image files for the native loader. Files disappear with the host.
Missing agent images and user photos use initials/fallbacks; no user-photo API is
invented. Sandbox terminal IDs/credentials and SSE transport stay in the engine;
Luau sees opaque local terminal handles and uses the existing terminal panel.

Desktop VNC and Browser JPEG-frame viewers are **deferred**. Their coming-soon
labels do not request capabilities, start WebSockets, or expose bearer URLs.
See [the GPUI feature inventory](docs/reference/keiki-features.md).

## Retained transport debt

The deep sync/DeviceRoom/chat2 purge is **deferred**, including DocHost,
WorkspaceHost and `crates/sync`. These supply retained storage/compatibility code,
but the Local-only scope and AuthTokenSource (which never supplies an account
transport token) keep account relay/sync paths inert. Do not interpret archived
Synced/Development stores or retained Rust types as supported online profiles.
No migration or deletion of those archives is part of this product.

## Release and update boundary

GitHub Actions builds the engine and the separate ZGX workspace from one commit.
Linux packages contain the app, helper engine, Luau, builtin plugins and installer;
macOS packages contain the same resources in `Keiki.app` (macOS 13+). Artifact
names are `keiki-<version>-<platform>-<arch>.*`. Linux x86_64/aarch64 and macOS arm64
are the default release set. Windows retains Rust checks and an optional ZGX build,
but is not in the default release matrix.

The Comet GitHub Releases API is the default feed, not Zeron's edge or R2.
`KEIKI_RELEASES_URL` can override it with a GitHub release API or a manifest base.
A `v<version>` release supplies tags/assets plus checksum `manifest.json`; old
Comet GitHub assets without that manifest can use GitHub's digest metadata.
The app uses API asset URLs, keeping optional private-release credentials in Rust.
Beta opt-in reads GitHub's published releases (including stable), never an edge
channel bucket. Default stable follows GitHub's latest non-prerelease release.

Existing managed-install transactions, checksum verification, macOS minimum-version
holds and trial/rollback machinery remain. Linux updates stage a version directory
before switching `current`; macOS uses an app tarball for the bundle swap. A DMG
is also published for user downloads and the former GPUI updater. Source/unmanaged
builds link to Comet's GitHub release page rather than trying to overwrite themselves.

Release compatibility still needs a policy for existing GPUI installations on
macOS 12: the old bundle declared a 12.0 floor and its updater does not read the
new minimum-OS manifest. Reusing its DMG asset name in GitHub's latest release
would offer those installations a macOS-13-only app. New ZGX clients enforce the
manifest floor, but cannot retroactively protect an already-shipped GPUI client.

## Verification

Fake Keiki HTTP/OAuth/SSE fixtures cover scope races, redaction, browsing, history,
actions and terminal lifecycles. Luau fixtures cover the gate, grouped navigation,
streaming, questions and controls. CI retains engine/core and ZGX Rust/Luau tests;
strict `zgx check ui` is a separate manual check with inherited SDK/template errors.
Shipping-contract tests validate the complete artifact set and checksums. Release
publication requires all default macOS/Linux packages; neither the edge nor the
former native Codex runtime is a release dependency.
