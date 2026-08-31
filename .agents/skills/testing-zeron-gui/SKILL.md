---
name: testing-zeron-gui
description: Run and GUI-test the zeron desktop app (GPUI) headed on a Linux/X11 box, including the Keiki OAuth loopback sign-in flow. Use when verifying UI changes, sign-in gates, theming, sidebar organization, or Keiki agent/conversation data in comet.
---

# Testing the zeron desktop shell headed on Linux/X11

## Build & run

```bash
cd /home/ubuntu/repos/comet
cargo build                     # cold build ~21 min; reuse target/
DISPLAY=:0 ./target/debug/zeron
```

Useful env vars (grep `std::env::var` in `crates/ui/src` if these drift):

- `ZERON_FORCE_GATE=signin` — legacy: since comet's own cloud sign-in was removed there is no
  sign-in gate, and this var is a no-op. Sign-in/out live in the bottom-left account chip menu, so
  one process now covers signed-out → signed-in → signed-out without the two-stage dance below.
- `ZERON_OPEN_ROUTE=settings/appearance` — deep-link into a settings section.
- `RUST_LOG=info,zeron_ui=debug,keiki_api=debug` — logs OAuth discovery/registration bodies.
- Delete `~/.zeron/ui-settings.json` to get true first-run defaults (needed for theme-default tests).

Window management: `DISPLAY=:0 wmctrl -lG` (the zeron window shows as `N/A`),
`wmctrl -i -a <id>` to focus, `wmctrl -r :ACTIVE: -b add,maximized_vert,maximized_horz` to maximize.
Do **not** use `xdotool key super+Up` (tiles to half screen).

Cleanup: `pkill -x zeron`. Never `pkill -f zeron` — it matches and kills your own shell command.

## Secret Service / keyring is required for sign-in persistence

GPUI writes credentials through the DBus Secret Service. A bare cloud box has no user DBus
session, so sign-in fails with:

```
Keiki request task failed: credential write: DBus error
zbus error Failed to connect to address `unix:path=/run/user/1000/bus`: No such file or directory
```

Workaround wrapper (`apt install gnome-keyring libsecret-tools` first):

```bash
#!/bin/bash
set -e
export DISPLAY=:0
export XDG_RUNTIME_DIR=/tmp/xdgrt
mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
cd /home/ubuntu/repos/comet
exec dbus-run-session -- bash -c '
  eval "$(printf somepassphrase | gnome-keyring-daemon --unlock --components=secrets 2>/dev/null)"
  export GNOME_KEYRING_CONTROL SSH_AUTH_SOCK
  exec "$@"
' bash "$@"
```

Each `dbus-run-session` gets its own keyring daemon. Credentials written during a session are
restored by a *later zeron process inside the same session* (`cx.read_credentials("keiki://oauth")`
at startup), but do not count on them surviving across separate wrapper invocations — plan to
re-do the OAuth flow per wrapper session.

## Two-stage launch (legacy — pre-account-chip builds)

On builds that still had comet's sign-in gate, the "Sign in to Keiki" button only existed there and
`ZERON_FORCE_GATE=signin` was sticky, so the gate never dismissed and two processes were needed:

```bash
/home/ubuntu/run-zeron-keyring.sh bash -c '
  export RUST_LOG=info,zeron_ui=debug,keiki_api=debug
  ZERON_FORCE_GATE=signin ./target/debug/zeron > /tmp/gate.log 2>&1   # sign in, then close window
  ./target/debug/zeron > /tmp/shell.log 2>&1                          # restores token, real shell
' &
```

Close the first window with `wmctrl -i -c <id>`; the second launches ~15 s later and comes up
signed in. Re-signing in after "Sign out of Keiki" needs the same two-stage dance (the account
menu offers no "Sign in to Keiki" row once signed out).

## Keiki OAuth loopback flow (RFC 8252)

`register_url_scheme` is unimplemented on GPUI Linux, so the app binds `127.0.0.1:0` and registers
`http://127.0.0.1:<port>/oauth/callback` dynamically. Expect per-attempt: a new port, a new
`mcp_...` client_id from `/oauth/register` (201), and a browser consent page "Connect Keiki Desktop?".
Verify no leaked listeners afterwards with `ss -ltn | grep 127.0.0.1`.

The gate button becomes a non-clickable "Opening Keiki…" while a flow is in progress, so rapid
repeat clicks are inert by design (only one tab opens). Denying consent returns
`?error=access_denied` and the gate renders `authorization was rejected: access_denied` in red.

## Gotchas seen in the Keiki layer

- Keiki agents surface as **projects** in the sidebar project picker (`@ Keiki` badge), not just as
  group headers; conversations only appear once they map to `Chat`s.
- `crates/ui/src/keiki.rs::parse_timestamp` must handle Postgres-style timestamps
  (`2026-08-28 16:58:29.714004+00`) as well as RFC 3339; a parse failure used to silently drop the
  whole conversation. It now falls back to `Utc::now()` and logs
  `Keiki timestamp could not be parsed` — always `grep -a` the log for that string, and if rows all
  show "now"-ish ages the parser has met a new format. Cross-check the raw JSON by opening
  `https://onkeiki.com/api/webapp/conversations` in the logged-in browser.
- Sidebar rows for Keiki chats show the conversation id (`tg:...` / `adam · #home · thread`) plus an
  `<agent> @ Keiki` subtitle; they do **not** render `last_message_preview`. Judge "previews" from
  the transcript, not the row.
- Agents with zero conversations get no sidebar folder but still appear in the project picker, so
  "5 agents, 4 folders" can be correct — count against the API payload.
- Read-only check: the composer send button is dimmed (`opacity 0.35`) for `keiki-conv:` chats;
  Enter and clicking it must be no-ops that leave the typed text in place. Confirm nothing escaped
  to production by re-reading `messageCount` for that conversation from the API.
- "Sign out of Keiki" must clear the `keiki-agent:` projects, the `keiki-conv:` chats and the account
  chip while leaving "All projects" — check sidebar, project picker and chip separately, they have
  failed independently before. (Settings → Devices no longer exists.)

## Stage-2 conversation controls (takeover / block / steer / agent creation)

- The per-conversation Keiki actions (**Take over / Hand back**, **Block / Unblock**) are reached by
  **right-clicking the selected conversation row**; since `1ad63ef` they sit on the **main** context
  menu page (between `Archive` and `Copy`) — older builds hid them under the `Copy` submenu. They
  only render for the row that is currently selected; check that before filing a bug.
- **"New Keiki agent…"** is in the sidebar header's agents dropdown (`All agents ⌄`) under
  `New agent…` (which opens a local folder picker), and also in the new-session canvas's project
  chip picker (`pickers.rs::render_space_popover`).
- Steering is **not** a dry run: `/steer` executes a real agent turn against the live conversation
  and persists internal rows (`messages.internal = true`); `messageCount` in
  `/api/webapp/conversations` increases and `lastMessage` changes. Nothing is delivered to the
  contact, but never describe steer as "no side effects", and expect the row count to move.
- Steer can fail with a verbatim upstream provider error (e.g. an OpenRouter 400) depending on the
  agent's model config — try a different agent's conversation before concluding the desktop is broken.
- Long server errors render in the composer notice area and can grow tall enough to push the input,
  Steer and send controls off-screen; switching conversations clears the notice.
- Transcripts of very large conversations (hundreds of messages) may not include the newest
  messages, so steer-recorded rows never show up there. Verify transcript freshness on a **small**
  conversation first, and cross-check the newest `lastMessage` against the API.
- Never press Enter / click send while a takeover is live: `Composer::on_submit` routes to
  `keiki::send` (real delivery) whenever `send_blocked` is false. Do gating tests before taking over.

## Stage-3: Keiki agent settings / delete, and the MCP scope upgrade

- **Where the agent management lives:** sidebar header `All agents ⌄` → **right-click an agent row**.
  Keiki rows (`keiki::is_keiki_space`) get exactly `Agent settings…` + `Delete agent…`; local rows
  get `Rename…`/`Delete…`. There is no left-click path — a left click just filters the sidebar.
- The settings dialog's scroll container (`max_h(600) + overflow_y_scroll`) only scrolls when the
  cursor is **over the dialog's own column, not over an inner list**. If the Features toggles look
  cut off, move the pointer over the feature rows and scroll again before reporting clipping.
- The `Line` picker only offers real org lines, so **an "invalid assigned line" cannot be produced
  through the UI when `GET /api/webapp/lines` returns `{"lines":[]}`** (check that endpoint first).
  Substitute another failing save to exercise the error path: the reliable, side-effect-free way is a
  temporary `sudo iptables -A OUTPUT -d $(getent hosts onkeiki.com | awk '{print $1}') -j REJECT`,
  attempt Save, then delete the rule. The dialog keeps the verbatim text above Cancel/Save.
- The server does **not** validate `model` on agent update: saving a nonexistent id such as
  `google/gemini-does-not-exist` returns 200 and persists. Useful as a round-trip probe (reopen the
  dialog — it reloads from `load_agent_config`), but always restore the original value afterwards.
- The prompt textarea's text can visually overflow onto the `Max steps` / `History limit` fields in
  the settings dialog — cosmetic, may still be present; don't mistake it for a load failure.
- **MCP scope upgrade:** account row at the bottom of the sidebar → `Connect Keiki tools…` runs the
  same loopback OAuth flow with `scope=mcp manage` (verify in the consent URL's query string) and a
  `resource=…/mcp` parameter; after the grant the row becomes a dimmed, non-clickable
  `Keiki tools connected`. A plain `Sign in to Keiki` requests `scope=manage` only, so after a
  sign-out/sign-in cycle the row correctly reverts to `Connect Keiki tools…` — that is expected, not
  a regression. Always re-check that agents/conversations still load after any token replacement.
- Deleting an agent: the confirm card is titled `Delete Keiki agent?` and quotes the agent name in
  the body — read the name before confirming when working against a live org. The sidebar row
  disappears immediately (snapshot refresh, no poll wait); cross-check `/api/webapp/agents`.

## Stage-5: Keiki account chip, and provoking an approval card on production

- The bottom-left chip **is** the Keiki session (`GET /api/webapp/auth/session`). The pass bar is the
  real name **and** the real active-org subline: a failed session fetch degrades to a plausible-looking
  bare `Keiki account` while still signed in, so "it says Keiki" is not evidence. It must never read
  `Local only` — that string, Settings → Devices, and the update strip are all gone with comet's cloud.
- Local state now lives under `~/.zeron/profiles/local` (`docs.sqlite3` + `journals`); an
  `orgs/<org>/<user>` path appearing means account-scoped profiles came back.
- **Only `run_approved_program`-class operations raise an approval card** (agent deletion, key
  rotation, role/ownership change — sms-kit `copilot/approved.ts` `needsApproval`). A rename or config
  update is **not** gated: it goes straight through `POST /agents/:id` and executes, so never use a
  "no-op rename" as a safe card probe — it is a real write to real data.
- A **nonexistent** target never reaches the approval path either: the copilot verifies the org first
  and refuses, creating no interrupt row.
- So on a live org the only safe probe is a **purpose-built throwaway agent**:
  `POST /api/webapp/agents` needs only `name`/`model`/`systemPrompt`, then ask the copilot to delete
  *that* agent, decline, and clean up with `DELETE /api/webapp/agents/:id`. Confirm the target before
  clicking by reading the literal program out of `copilot_interrupts.payload`.
- Declining leaves the run as `status = interrupted` with a **separate** resume run reaching
  `completed` — `interrupted` is not a failure. Verify the effect, not the prose: re-read the target
  read-only afterwards.
- Production read-only replica: `secret:repo:Mail-0/sms-kit:SMS_KIT_PROD_DATABASE_URL_RO`. Useful
  columns/tables when cross-checking: `copilot_runs.started_at`, `copilot_interrupts.requested_at`,
  `users.org_id`, `organizations`.

## Stage-4: the Copilot harness (local sms-kit)

- Run the server from `/home/ubuntu/repos/sms-kit` with its `.agents/skills/testing-platform-web`
  recipe (Postgres in Docker + platform API + built SPA on `http://localhost:8080`), then launch the
  desktop with `KEIKI_API_URL=http://localhost:8080`. Production may not carry the copilot bearer
  change, in which case `/api/copilot` 401s — always prefer the local stack for Copilot work.
- The `Copilot` row is pinned above the agent groups (`shell/spaces.rs`
  `render_copilot_launcher_row`). Signed out it is drawn at `opacity(0.45)` and clicking it only sets
  the sidebar notice `Sign in to Keiki to use Copilot`. Signed in it reuses
  `copilotChatId` from `~/.zeron/ui-settings.json` (persisted immediately) — check that file to prove
  a relaunch reused the same chat rather than minting a new one.
- **Multi-turn context is server-sourced, and that is the fragile part.** Newer builds post one user
  message with `appendToTranscript: true` and the server prepends the thread's stored transcript
  (`copilot_transcripts`, ownership-checked); resumes post `messages: []` with `parentRunId` +
  `resume`. Always verify context *both* ways: ask in turn 2 something only turn 1 can answer (a
  token like `FALCON-4127`), and check the row grows —
  `select thread_id, jsonb_array_length(messages) from copilot_transcripts order by updated_at desc;`
  A count pinned at 2 means truncation even though the desktop transcript shows the whole thread.
  Older builds fetched history from `GET /api/copilot/threads/:id` (the *dashboard's*
  `copilot_threads` store, which the desktop never writes → 404 → empty history); if you see any
  `threads/` request in the API log on a modern build, that regression is back.
- **Approval / HITL:** a destructive prompt (deleting an agent) persists a `copilot_interrupts` row
  (`status='pending'`) and sets `copilot_runs.status='interrupted'`. Newer clients hydrate
  `GET /api/copilot/chat?threadId=…` and read `interrupts.{runId,pending}` at turn start *and* after a
  seemingly clean completion, raising the interrupt through the composer wizard
  (`crates/ui/src/composer.rs`): uppercase reason header, interrupt message as the question, and
  clickable `Approve` / `Decline` rows (`mod.rs` `await_interrupts` maps them to
  `{"approved": true|false}` + `Resolved`). Verify the *effect* server-side, not the UI text:
  `select interrupt_id,status,response::text from copilot_interrupts;` plus whether the target agent
  still exists. The turn-start hydration is also the recovery path for a thread wedged by an older
  pending interrupt — just open it and send any prompt; the old card should appear, and after the
  resume the typed prompt is sent as its own fresh turn.
- **Premature `RUN_FINISHED` when the copilot delegates to its `code_task` sub-agent.** In those runs
  the SSE log shows *two* `RUN_FINISHED` frames, the first arriving **before** the assistant's
  pre-approval prose and before the interrupt is persisted. The client ends the turn at that frame, so
  the turn renders only a collapsed `Thought · Called 1 tool` row: the prose never appears (not
  "appears then vanishes"), and no approval card is raised for that turn. Detect it with
  `select run_id, count(*) filter (where chunk->>'type'='RUN_FINISHED') rf,
   min(case when chunk->>'type'='RUN_FINISHED' then seq end) first_rf,
   min(case when chunk->>'type'='TEXT_MESSAGE_START' then seq end) first_text
   from copilot_stream_chunks group by run_id;` — `rf = 2` and `first_rf < first_text` is the bug;
  `rf = 1` with `first_rf > first_text` is a healthy run whose prose renders normally. The prose
  always arrives as `TEXT_MESSAGE_CONTENT` (→ `TextDelta`), never as a reasoning event, so this is a
  stream/turn-termination problem rather than a reasoning-mapping one.
  Fixed builds read `metadata.tanstack.finishReason` (top-level `finishReason` as fallback) and treat
  `"tool_calls"` as **non-terminal** while `outcome.type == "interrupt"` still wins. To prove a fix
  rather than a lucky non-delegating path, always re-run the provenance query for the *new* run and
  require `rf = 2` **and** `first_rf < first_text`; if the copilot answers without delegating
  (`rf = 1`) force the delegation by naming it in the prompt, e.g. "Use your code_task sub-agent to
  delete the agent named X", and check the finish reasons per frame with
  `select seq, chunk->'metadata'->'tanstack'->>'finishReason', chunk->>'outcome' is not null
   from copilot_stream_chunks where run_id='…' and chunk->>'type'='RUN_FINISHED' order by seq;`
- **"Blank main pane / no composer" is almost always ZERO SPACES, not a rendering bug.** The composer
  is rendered only `.when(has_spaces, …)` (`crates/ui/src/shell.rs`, `render_main`; `has_spaces =
  !state.spaces.is_empty()`), and a selected chat with an empty transcript paints a *bare* canvas by
  design. So if the local org's `agents` table is empty (Keiki agents are the spaces), **every** chat —
  including a brand-new Copilot chat in a fresh profile — shows an empty canvas with no composer, and
  the app looks permanently wedged. This is the trap when testing the approval flow: **approving a
  deletion of the LAST agent removes the composer for the rest of the session.** Always keep a second
  agent around (e.g. seed `Delete Me X` + `Keep Me`) so the post-resume usability checks are runnable.
  Diagnosis takes seconds: `select name from agents;` — if it is empty, seed one
  (`insert into agents (org_id, api_key, name) values ('<org>','<dev api key>','Keep Me');`) and the
  composer returns within one poll cycle, no restart needed. Symptoms that are *consistent* with this
  and should not be mistaken for a crash: sidebar renders/updates normally, header shows the session
  title, 0 panics, `engine core assembled` logged normally, relaunches / archiving the chat / clearing
  `copilotChatId` all change nothing. Only after ruling this out should you suspect the composer wizard
  replacing the input (`crates/ui/src/composer.rs`: the wizard renders *instead of* the composer, and
  `render_wizard` returns `Empty` with no current question) or the build itself — and confirm
  `git status` is clean before blaming the commit under test, since uncommitted WIP in `crates/doc`,
  `crates/engine`, `crates/sync` or `crates/ui` is compiled into `target/debug/zeron`. For a
  guaranteed-clean binary, build from a separate worktree (e.g. `/home/ubuntu/comet-clean`) rather than
  stashing in the main checkout; a cold `cargo build` there takes ~40 min.
- The composer's visibility is gated in `crates/ui/src/shell.rs` (`composer_visible(has_selection,
  has_spaces)` on newer builds, previously just `.when(has_spaces, …)`). On builds where the gate is
  `has_spaces` only, a Copilot chat is completely unusable when the org has no agents (no composer at
  all) — check `select name from agents;` before blaming the UI. Newer builds render the composer
  whenever a chat is selected, so zero agents no longer blocks the Copilot.
- Reaching the "no chat selected" state and the onboarding card is fiddly, and the two states have
  *different* preconditions:
  - No selection = the titlebar `+` (`open_new_session` → `state.select_chat(None)`), top-left ~(71,12).
  - The onboarding card ("Add an agent to get started") additionally needs `no_project == false`.
    Selecting the **project-less Copilot chat** sets `no_project = true` at runtime
    (`state.rs`, `select_chat` → chat with `space_id: None`), and `select_chat(None)` deliberately
    keeps that project pick — so after ever opening the Copilot chat, `+` yields a *bare* canvas, not
    the card. This is expected, not a bug.
  - To get the genuine first-boot state without a full re-sign-in (credentials live in the system
    keyring, the profile is `~/.zeron`): archive the remaining chats via the sidebar row's `Archive`
    button, remove `copilotChatId` from `~/.zeron/ui-settings.json`, ensure `noProject: false` in
    `~/.zeron/composer-defaults.json`, then relaunch so nothing auto-selects. Note a relaunch will
    auto-select the most recent non-archived chat, which is why archiving is the necessary step.
- Seeding an agent for the "normal Keiki case": `insert into agents (org_id, api_key, name) values
  ('<org>','<dev api key>','<name>');` — every other column has a default. The snapshot poll takes
  ~20-40 s. With `sidebarOrganization: "inOneList"` the agent does **not** appear as a sidebar header;
  it shows in the agent picker (click the folder chevron at ~(127,38)) tagged `@ Keiki`, and picking it
  re-scopes the sidebar header to that agent.
- Getting a cancellable run is harder than it looks: the Keiki copilot **refuses** generic long-form
  prompts ("write 800 words about…", "count to 300") and answers in ~5 s, too fast to hit stop. Use a
  `code_task`-delegating prompt (e.g. "Use your code_task sub-agent to fetch every agent in this org
  and report each one's full configuration in detail") — it streams for 30-60 s, giving a wide window
  to click the composer stop control.
- Cancel: the composer stop control halts the stream and re-enables the composer. On newer builds the
  client posts `POST /api/copilot/runs/<uuid>/cancel` (the run id it minted in the POST body) → 200,
  and `copilot_runs` shows `cancel_requested = t` with status `aborted`. Older builds sent the
  provider message id (`openrouter-…`) and 404'd, so always read both the API log line and the row
  before calling cancel "clean". Note the next turn after a cancel may answer the *cancelled* prompt,
  since the aborted user message is in the stored transcript.
- Useful server-side evidence tables: `copilot_transcripts`, `copilot_runs`, `copilot_interrupts`,
  `copilot_stream_chunks` (`select chunk->>'type', chunk->>'name' …` reconstructs the exact AG-UI
  event sequence a turn produced — the fastest way to tell "server never sent it" from
  "client never rendered it").
- After the harness cull, Settings has exactly Devices / Appearance / Notifications / Shortcuts /
  Archived sessions, and the new-session canvas has only a device picker and an agent picker — no
  harness selector. A fresh local org has no conversations, so pin / `View conversation` / send-gating
  checks need either seeded conversations or the production org.

## Devin Secrets Needed

- A Keiki account (email + password) for signed-in testing; `KEIKI_API_URL` defaults to
  `https://onkeiki.com`. Type passwords with `xdotool type --file <path>` so they never appear in
  logs or screenshots.
