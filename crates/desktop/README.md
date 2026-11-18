# Keiki desktop

The shipping `zeron` headed launch uses this ZGX/Luau library. `zeron headless`
and `zeron daemon …` continue to use the same local engine and profile.
Build with `cargo build --locked -p zeron` from the repository root.

## Bridge contract

Rust owns I/O, credentials and domain state. Luau owns rendering and transient
input state. Rust publishes one reactive JSON value per domain:
`keiki.account`, `keiki.conversations`, `keiki.transcript`, `keiki.workspace`,
`keiki.copilot`, `keiki.sandbox`, `keiki.settings`, `keiki.updater`.
Each has a matching Rust module in `src/` and Luau module in `ui/`.
Actions use `<domain>.<verb>` names; never unqualified action names.

Current actions: `account.sign_in`, `account.sign_out`, `account.choose_org`,
`conversations.choose_agent`, `conversations.send`. Payloads are strings.
Commands execute serially; busy state prevents overlapping account operations.
Account snapshots carry authentication/org/agent/error state; conversations
carry selection; transcript carries the latest streamed reply; workspace
carries connection status and names. Copilot/sandbox/updater publish
`available = false` until their later parity stages; they expose no fake actions.
Snapshots contain no tokens or provider credentials.

The build embeds the Luau bundle. Tests lint the scripts and exercise the
bundled view through ZGX TestApp. Run strict Luau analysis with
`cargo run --locked -p zeron-desktop --example check-luau` (requires luau-lsp).
GPUI remains in `crates/ui` as an unlinked reference during the parity cutover.

## Lifecycle

The host probes localhost IPC, attaches to an existing daemon, or embeds the
same Engine runtime and exposes its IPC service. A lost workspace subscription
reconnects, falling back to embedding if the daemon has stopped. Closing the
window or interrupting the process drains an embedded runtime; an attached
daemon remains running.

OAuth credentials use the existing `keiki://oauth` internet-password entry on
macOS (account `OAuth`) and the existing Secret Service URL/label on Linux.
Startup refreshes a saved session and stores rotated refresh tokens. Sign-out
and rejected refresh credentials delete the entry. Storage errors surface as
account errors; credentials never fall back to plaintext files.

`ZERON_OPEN_ROUTE` populates `keiki.settings.route`. Launch URLs and macOS
`keiki://` open events populate `keiki.settings.deep_link`; navigation is deferred
to later stages. OAuth callbacks remain on the loopback listener and are never
published. URL queries/fragments are stripped from state.
