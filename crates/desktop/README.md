# Keiki desktop

The shipping `zeron` headed launch uses this ZGX/Luau library. `zeron headless`
and `zeron daemon …` continue to use the same local engine and profile.
Build with `cargo build --locked -p zeron` from the repository root.

## Bridge contract

Rust owns I/O, credentials and domain state. Luau owns rendering and transient
input state. Rust publishes one reactive JSON value per domain:
`keiki.account`, `keiki.conversations`, `keiki.transcript`, `keiki.workspace`,
`keiki.composer`, `keiki.copilot`, `keiki.sandbox`, `keiki.changes`,
`keiki.settings`, `keiki.updater`.
Each has a matching Rust module in `src/` and Luau module in `ui/`.
Actions use `<domain>.<verb>` names; never unqualified action names.

Current actions: `account.sign_in`, `account.sign_out`, `account.choose_org`,
`conversations.choose_agent`, `conversations.send`. Payloads are strings.
`composer.clear` clears the shared draft string (`keiki.composer`).
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

## Parallel domain ownership

Each engineer owns `src/<domain>.rs` (and its submodules) plus
`ui/<domain>.luau`. Every domain has a `register(&Registration)` entry point
already called by the host; extend that function to publish snapshots, register
actions and spawn UI-side subscriptions without editing `lib.rs`.
Account and workspace workers retain their lifecycle functions in their own
modules. `bridge.rs` is shared registration infrastructure, not domain logic.

| Domain | Owned Luau mounts |
| --- | --- |
| account | `sidebar`, `header`, `status`, `modals` |
| conversations | `sidebar`, `tabs` |
| transcript | `main` |
| composer | `main` |
| workspace | `sidebar` |
| copilot | `sidebar` |
| sandbox | `panels` |
| changes (diff, Git history, PR badges) | `panels` |
| settings | `overlay` |
| updater | `modals` |

Mounts return arrays of elements; empty arrays render nothing and reserve no
space. `app.luau` only places these mounts into named regions: `left_sidebar`,
`main_area` (including the conversation tab strip), `right_bottom_panels`,
`settings_route_overlay`, and `modal_layer`. Fill an existing mount rather than
editing the shell. `kit.children` flattens mount arrays without adding layout
wrappers, keeping the current layout unchanged.

Cross-domain data flows **only through published snapshots and actions**.
A domain may read another domain's `keiki.*` shared value; it must not import
another domain's view/state implementation or mutate that domain's snapshot.
Send `<domain>.<verb>` actions to request changes. Composer owns its writable
draft and clear action. The existing account-backed conversation/transcript
projections and command adapter in `bridge.rs` are transitional host wiring;
do not grow new direct domain-to-domain dependencies.

`ui/kit.luau` owns shared button, list-row, text-input and icon helpers plus
color/spacing tokens. Reuse them. Changes to `lib.rs`, `bridge.rs`, `app.luau`
or `kit.luau` require coordination with the integration engineer; domain
branches should otherwise be additive and limited to their owned files.

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
