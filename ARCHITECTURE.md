# Keiki Desktop architecture

## Product boundary

Keiki Desktop is a native GPUI client for the server-hosted Keiki platform. It
does not run coding agents, maintain worktrees, host terminals, persist CRDT
documents, or synchronize devices.

Postgres-backed Keiki services remain authoritative. The desktop app consumes
authenticated HTTP APIs and will consume server-sent events for live activity.

## Workspace

```text
apps/keiki       Native desktop executable
crates/api       Keiki HTTP client
crates/model     API-facing desktop models and presentation ordering
crates/ui        GPUI application shell
crates/theme     Resolved theme domain and import tooling
crates/syntax    Tree-sitter syntax highlighting
```

## Current milestone

The application opens a native window containing:

- an agent sidebar;
- attention-first agent ordering;
- an empty conversation state;
- Keiki-branded menus, settings, URL scheme, and packaging;
- a normalized request builder for `/api/webapp/agents`.

The API client is deliberately transport-only at this stage. Authentication,
response decoding, retries, and streaming belong in later milestones once the
Keiki endpoint contracts are finalized.

## Planned surfaces

1. OAuth 2.1 sign-in with the `manage` scope.
2. Agent listing, selection, editing, and creation from templates.
3. Live Test conversations with streamed activity.
4. Conversation history, messaging, and human takeover.
5. Trace inspection.

## Upstream boundary

The fork retains comet's GPUI presentation foundations and theme/syntax
infrastructure. The local-first engine, harness adapters, CRDT document layer,
device synchronization, RPC server, edge worker, terminal integration, iOS
client, and landing site are intentionally removed.
