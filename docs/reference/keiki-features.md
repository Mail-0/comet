# Keiki feature inventory and porting boundaries

The reference is Comet's `origin/main` GPUI implementation, not the presence of
an API helper alone. Credentials and capability URLs stay in the engine.

The ZGX column is the intended treatment, not a completion checklist.

| GPUI feature | Existing contract | Intended ZGX treatment |
| --- | --- | --- |
| Rename, archive/restore, delete | Generic local engine `Mutate`; no remote rename/archive/delete call | Local bound-chat metadata and deletion of the local copy only; remote history is not deleted |
| Pin/unpin | Local settings | Account/org-scoped binding preference |
| View conversation | Public dashboard URL, identity + optional agent ID; no API key | System browser, opened by engine |
| Copy conversation ID | Locator identity, not a credential | Existing clipboard component |
| Block/unblock | Conversation `/block`, POST/DELETE | Engine action |
| Take over/hand back | Conversation `/takeover`, POST/DELETE | Engine action, contact conversations only |
| End peer thread | Conversation `/end`; settles waiting asks | Engine action, `agent:` conversations only |
| Group roster | Group `/roster`; roles, state, counts, usage, cost | Existing list and badge components |
| End group work | Group `/end`, with confirmation | Engine action; not a group deletion |
| Background tasks | Conversation detail `tasks`; task `/cancel` | Existing list and confirmation components |
| Start swarm / reassign members | No GPUI action or API contract | Not invented |
| Terminal | Open/input/resize/close HTTP calls and SSE `TerminalEvent` output | Engine transport, existing ZGX terminal |
| Desktop | `open_desktop` gives preview URL + preview token; GPUI uses native VNC over `/websockify`, authenticating with a header | Deferred by request; agent workspace shows “Desktop view coming soon”. No capability request or transport |
| Browser | Availability + screencast capability; native JPEG WebSocket viewer, credential in first message | Deferred by request; agent workspace shows “Browser view coming soon”. No capability request or transport |
| Agent avatars | Public avatar image, keyed by agent/state/theme/size; bounded cache with fallback/retry | Existing avatar component; image bytes fetched through engine. Rust bridge keeps at most 64 temporary image files because ZGX accepts paths, not data URIs; files are removed when the host is dropped |
| User avatar | First-letter fallback in GPUI; session has no photo field | Existing user avatar fallback; no invented photo endpoint |
| Create agent | List templates, create from selected template; optional name/line | Existing form/dialog components |
| MCP connection prompts | Connect and refresh preset status; open provider authorization page | Engine opens authorization URL; Luau receives status only |

GPUI evidence: `crates/ui/src/keiki.rs`, `shell.rs`, `shell/spaces.rs`,
`shell/threads.rs`, `composer.rs`, `avatars.rs`, `terminal/transport.rs`,
`desktop.rs`, and `browser.rs` at `origin/main`.

Local deletion must not call the API's clear-history operation: GPUI did not use
that operation. Remote conversation identities are not local chat IDs, and a
removed local copy can be imported again from Keiki.
