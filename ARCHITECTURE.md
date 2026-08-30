# zeron — Architecture

A ground-up native rewrite of [zeron](../zeron) — a multi-device controller for coding agents
(Claude Code / Codex) — in Rust, with a gpui UI. Fresh app; no backwards compatibility required.

**Pillars (from the goal):**
- Loro CRDT docs and the workspace index persist locally in SQLite; the desktop has no
  Comet account or cloud-sync dependency.
- Keiki is the only provider account and agent surface. Everything device-side is Rust.
- Feature parity with zeron **except token-usage display** (poor fit for CRDTs; excluded).
- Frontend is **gpui** (pinned Zed rev). Virtualization + markdown techniques ported from
  **mugen + pretext** (`docs/research/mugen-pretext.md`).
- One binary, **headed or headless**. Smooth transitions/animations matching the original
  (catalog in `docs/research/feature-inventory.md` §1.12).

## 1. Topology (unchanged shape, new materials)

```
gpui UI ─ in-proc/localhost RPC ─ engine ─ local SQLite/Loro docs
                    │
                    └── optional localhost RPC ── zeron daemon
```

- **Engine = backend** (was `@zeron/backend`): runs agents, owns terminals, repos/worktrees,
  diff watching, and local doc hosting. Pure Rust daemon, fully functional headless.
- **UI = viewport** (was Electron): gpui app rendering engine state. Talks the same typed RPC whether the engine is in-process or a separate daemon. Organized around **spaces** — (device, folder) pairs. The sidebar is the data: an attention-sorted Sessions list, filtered by a searchable spaces dropdown ("All spaces" included) that also hosts space management. The horizontal tabs are a **device-local viewport** onto that list (`ui-settings.json` `openTabs`, cross-space): closing a tab is local-only — archiving is an explicit sidebar action — and a sidebar click (re)opens a session as a tab. The new-session canvas carries a space picker (defaulting to the sidebar filter, else the last selected space).
- **Keiki**: the web account and OAuth provider used by the UI and Copilot. It is not a
  transport for local workspace documents or engine sessions.

### Headed / headless
Single binary `zeron`:
- `zeron` — headed. If a local engine daemon is already listening on the IPC port, connect to it;
  otherwise run the engine **in-process** (RPC over an in-memory duplex — same protocol, zero
  serialization shortcuts, so the boundary stays honest) **and serve that same engine on the IPC
  port**. The embedded engine is not private: any other viewport can attach to the running app
  without it first being restarted as a daemon. Binding is best-effort — if the port is taken the
  window still opens, having lost only the ability to host peers.
- `zeron headless` — engine only. A clean installation immediately serves its local
  profile over localhost IPC. A separate UI can attach to it through the same local
  protocol.

### Local workspace profile

The engine always opens one device-local profile. Session documents, the local workspace
index, run journals, and uploads live under `{data_dir}/profiles/local/`; local identity
is stored in `{data_dir}/local-profile.json` and remains stable across restarts.

Device identity and machine resources remain device-scoped under the common data directory:
device id, repository registration, managed worktrees, agent credentials, and UI settings.

## 2. Data model — all Loro

Two persistent doc kinds live in the local profile:

1. **Session doc** (per chat) — the transcript + durable command queue. Schema is a Rust port of
   `packages/session-doc`): `meta` map, `messages` list (parts as list-of-maps with **LoroText bodies** — the
   measured 1.03× oplog shape; never LWW value rewrites), `commands` list with ledger rules 1–3
   (append-only per-device entries; host-only outcomes; dedupe/TTL/supersede evaluation).
   Continuation splitting at 256KB, render-only tool parts (full inputs stay in the host's local
   run journal). Constants carried over (`STREAM_COMMIT_MS=120`, compaction at 8MB,
   retain 30d).

2. **Workspace registry doc** (per profile) — the `registry1` snapshot stores spaces (id, deviceId, path, name?, gitDetected, checkoutId), the chats index (id, deviceId, title, archived, cwd, branch, checkoutId, spaceId, lastSeenAt, lastMessagePreview/At, config), devices, session-status rows, and checkout-diff summary pointers. A space is a device+folder pair in the active profile; the owning device's `SpacesSync` stamps git presence so branch pickers and the diff sidebar can gate without another RPC. The registry is entirely local.

   Writer discipline: the local engine writes its device and session-status rows, rows for
   chats it hosts, and git stamps for spaces it owns. The sidebar needs one subscription
   for the whole list (grouping, resort animations, unseen markers), so the index remains
   one local snapshot.

3. **Mirror layer** (`zeron-doc` crate) — Rust equivalent of loro-mirror: typed structs for the
   schema, **incremental** application of `doc.subscribe` diffs into cached state (no full
   re-hydration per change — this is also what fixes zeron's known O(transcript) re-projection
   inefficiency, remaining-work item 1a), and a diff-reconcile write path (evaluate `lorosurgeon`
   0.2.x as a dep; our schema is small enough to hand-roll if it doesn't fit). The UI renders
   mirror state directly with per-entry change notifications — the "endgame" the TS
   implementation documented but never reached.

### Command plane
Send/steer/interrupt/respondInput = durable command entries in the session doc (`QueueCommand`),
executed by the chat's **host** device (executor gated on chat ownership; mark-processed BEFORE
execute; steer with no live run dispatches as the next turn). Offline sends queue in the doc.
This is zeron's proven design, kept verbatim.

## 3. Cargo workspace

```
zeron/
  Cargo.toml                 # workspace
  crates/
    proto/        zeron-proto    # wire types: AgentEvent, ToolCall, RunRequest, Model,
                                 # entities, RPC envelopes (serde; ndjson framing);
                                 # `view` = the pure derivations both frontends share
                                 # (sort orders, staleness gating, grouping, boot gate)
    doc/          zeron-doc      # session-doc + workspace-registry schemas, mirror layer,
                                 # parts fold, continuations, command ledger
    harness/      zeron-harness  # Harness trait + claude-code (stream-json subprocess),
                                 # codex (app-server JSON-RPC), mock; steering mailbox,
                                 # requestInput, models/reasoning/options catalogs
    engine/       zeron-engine   # sessions engine (pub/sub, run journal, recovery, stall
                                 # watchdog), doc host + command executor, repos/worktrees,
                                 # checkout-diff watcher, terminals (portable-pty), uploads,
                                 # agent accounts (cred swap), local identity
    rpc/          zeron-rpc      # UiRpc/ControlRpc: typed req/resp/stream over WS (tokio-
                                 # tungstenite) + in-memory transport
    theme/        zeron-theme    # source-neutral theme schema + built-in/custom registry,
                                 # validation, provenance, and local VS Code compiler
    ui/           zeron-ui       # gpui app: shell, sidebar, conversation, composer,
                                 # terminal view, diff pane, settings, animation kit
  apps/
    zeron/                       # the binary (headed default, `headless` subcommand)
  docs/                          # this file + research reports
```

Engine async runtime: **tokio** throughout; the UI bridges via `gpui_tokio` (`Tokio::spawn`
futures surfaced as gpui `Task`s). In-process mode runs the engine on its own tokio runtime
thread; the UI never blocks on it.

## 4. UI plan (gpui) — parity + smoothness

Reference: `docs/research/gpui.md`, `docs/research/mugen-pretext.md`,
feature spec `docs/research/feature-inventory.md` §1.

- **Deps**: `gpui` + `gpui_platform` pinned to one Zed rev (Apache-2.0). **We do not use Zed's
  GPL crates** (`markdown`, `ui`, `theme`, `editor`) — markdown, components, and theme are ours.
- **Transcript**: gpui `list()` + `ListState::new(n, ListAlignment::Bottom, overdraw)` (sum-tree
  offsets, follow-tail). On top of it, port the mugen behaviors that gpui doesn't give us:
  - stick-to-bottom **spring** with feed-forward tracking of streaming growth; interrupt from
    *user input* (wheel-up / drag), re-engage within a 70px band; own-send re-engages + smooth
    scrolls;
  - **block-granularity rows** (one row = one markdown block / tool group, not one message) with
    stable ids `msgId#blockId`; live turn stays unsplit, re-splits on persist; optimistic echo
    rows share the client-minted id so persistence never flickers;
  - row height memoization keyed by (row id, content length, width) so a streamed token
    re-measures one row;
  - scroll-anchor absorption for above-viewport height changes.
- **Markdown** (`zeron-ui::markdown`): `pulldown-cmark` parsing on `background_spawn` with
  coalescing (Zed's proven pattern), block-level incremental re-parse of the streaming tail
  (incremark's O(delta) idea: only re-parse from the last stable block boundary), monochrome
  theme where **numbers drive layout, colors are paint**. Code blocks: monospace, no wrap ⇒
  height = lines × line-height (layout independent of highlight); syntax highlighting via
  `synoptic`/`syntect`-class tokenizer run time-sliced in the background, colors applied as text
  runs (paint-only). Streaming **fade-in veil** on newly appended text via `with_animation`
  opacity (paint-layer, never affects layout). `prefers-reduced-motion` honored.
- **Composer**: hand-rolled gpui text input (start from Zed's `examples/input.rs`: IME, selection,
  clipboard, key actions), compact↔expanded auto-flip by measured text width, auto-grow 76–260px,
  Enter/Shift+Enter, Send→Steer→Stop morph, drafts + attachments per chat, drag-drop/paste
  images, QuestionPanel (paged, 1-9 keys, 220ms auto-advance) replacing the composer while input
  is requested. Pickers (harness/model, traits, repo w/ folder browser, branch w/ worktree
  toggle) as gpui popovers with `menu-in` scale/fade.
- **Terminal**: `alacritty_terminal` (vte state machine, MIT/Apache) + `portable-pty` on the
  engine side; custom gpui grid element; tabs w/ drag-reorder (150ms sliding transforms), height
  drag 160px–55vh, 12ms input coalescing / 80ms resize debounce, 1MB replay, detach ≠ close.
- **Diff pane**: unified-patch parser → virtualized file/hunk/line rows, per-file collapse
  (180ms height tween), time-sliced highlight, 200ms width transition on the pane itself.
- **Animation kit** (`zeron-ui::motion`): small helpers over gpui `Animation` reproducing the
  zeron catalog — `fade-in` (0.5s, cubic-bezier(0.16,1,0.3,1), translateY 4→0), `splash-out`,
  `zeron-pulse` staggered cell wave (boot splash + loaders), `gradient-spin-pulse` matrix
  spinner (WorkingIndicator + rotating flavour word), `menu-in`/`dialog-in` scale-fades, 200ms
  ease-out width/height transitions for sidebar/panes, sidebar-resort **slide animation**
  (we own the list, so animate row positions directly — the View Transitions equivalent, 260ms
  cubic-bezier(0.22,1,0.36,1)), reduced-motion switch.
- **Theme**: independent light/dark resolved variants, theme-owned semantic/syntax/terminal
  palettes, optional interaction-accent overlays, and a device-local surface preference that
  resolves each variant's recommended frost/opaque treatment without changing theme selection.
  Forced frost derives contrast-checked tints from mapped theme surfaces. Local VS Code
  file/package compilation and imported/linked custom families retain last-known-good
  persistence. Colors remain paint-only; hairline borders and bundled Geist/Geist Mono remain
  shared presentation foundations.

## 5. Engine plan

Direct ports of zeron behaviors (spec: feature-inventory §3):
- **Sessions engine**: per-session broadcast hub; on-disk run journal (resumable `seq` replay,
  crash auto-resume); persistent steerable sessions (steering mailbox at step/turn boundary; idle
  reaper; 10min stall watchdog); recovery stamps `aborted`.
- **Doc host**: per-chat handle (write user entries + stream assistant segments at 120ms commits,
  drain commands host-only with processed-ledger idempotence); SQLite snapshot store.
- **Harness** (research pending — `docs/research/harness.md`): trait mirroring zeron's
  `HarnessShape`; Claude Code via `claude` CLI stream-json in/out (control protocol for
  permissions/AskUserQuestion→requestInput, resume, steering); Codex via app-server JSON-RPC or
  `codex exec --json`; model/reasoning/option catalogs ported from `packages/harness`.
- **Repos/diffs**: git2 or `git` subprocess (subprocess — matches zeron, avoids libgit2 edge
  cases); worktrees under `~/.zeron/worktrees`; fs watchers (`notify`) + 2min repair; diff
  capture (patch + numstat + untracked, 3MiB cap, sha256) → workspace registry summary.
- **Agent accounts**: credential-slot swap (macOS Keychain via `security-framework`, files
  elsewhere), plan labels, usage probes, paste-code/browser-poll OAuth flows.
- **Auth**: Keiki OAuth in the UI, with device-local credentials used by the Copilot harness.

## 6. Cloud boundary

Keiki provides account/session and Copilot APIs. Comet does not host or join a document,
registry, device, or session relay; workspace state and engine sessions remain local.

## 7. Parity exclusions & deliberate changes

- **Excluded**: token-usage display (profile heatmap, lifetime stats, per-message token columns,
  `WatchUsage`). Rate-limit meters on agent accounts are *kept* (separate concern; probed from
  CLIs, not CRDT-synced).
- **Changed**: Postgres/entity sync and relay layers → local workspace registry; Electron/React/mugen
  → gpui with ported techniques; Node harness SDKs → subprocess protocols; mobile app → out of scope.
- **Kept verbatim**: session-doc schema shape, command ledger rules, render-parts privacy policy,
  UX behaviors, and animation timings.

## 8. Milestones

Status legend: ✅ shipped · 🟡 shipped with named gaps (see `docs/PARITY.md`).

- ✅ **M0 Scaffold** — workspace builds; `proto`/`doc` crates with ledger + parts + continuation
  unit tests; gpui hello-window runs.
- ✅ **M1 Doc core** — `zeron-doc` mirror over loro 1.13 with local snapshots and command ledger.
- ✅ **M2 Engine core** — Claude harness end-to-end headless: `zeron headless` + dev auth runs a
  turn, journal + doc writes, recovery test.
- ✅ **M3 UI core** — shell (sidebar/panes/header), transcript (virtualized, markdown, streaming,
  stick-to-bottom), composer (send/steer/stop, question panel); local chat fully usable headed.
- ✅ **M4 Local daemon** — a second UI can attach to a headless engine through localhost IPC;
  local sessions, workspace state, and command delivery remain usable without an account.
- 🟡 **M5 Full surface** — terminals, diff pane, repo/branch/folder pickers + worktrees,
  agent accounts UI, settings (devices/shortcuts/archived), Codex harness. Gaps: composer
  attachment UI (engine upload RPCs exist), Cursor harness.
- 🟡 **M6 Polish** — keyboard map, clippy/fmt sweep, Linux packaging
  (`scripts/package-linux.sh` + release profile), macOS bundling config (`dist/macos/`,
  not executed — needs a Mac). Gaps: prefers-reduced-motion and engine hardening
  (instance lock, watchdogs).

## 9. Open questions (tracked, non-blocking)

1. `lorosurgeon` fit for the mirror write path vs hand-rolled reconcile.
2. Cursor harness (zeron has it; CLI surface for Rust TBD) — parity item, scheduled after Codex.
3. Text shaping performance for analytic row heights: gpui measures shaped text natively (Rust ⇒
   cheap), so we start with gpui `list()` measurement + memoization rather than porting pretext's
   full analytic kernel; revisit only if cold-open of huge transcripts measures slow.
