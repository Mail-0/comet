# Engine Keiki conversations

Keiki browsing and execution use the engine's signed-in account and selected
organization. None of these RPCs accept credentials or return API keys or tokens.
Agent and group lists are whole lists, matching Comet; conversation and history
requests use Keiki's existing offset pagination.

| RPC | Parameters | Result |
| --- | --- | --- |
| `ListKeikiAgents` | `{}` | `{agents}` |
| `ListKeikiAgentGroups` | `{}` | `{groups}` |
| `GetKeikiAgentGroupRoster` | `{groupId}` | `{members, totals}` |
| `ListKeikiConversations` | `{agentId, limit?, offset?}` | `{conversations, nextOffset}` |
| `OpenKeikiConversation` | `{agentId, conversationId}` | `{chatId, binding}` |
| `NewKeikiConversation` | `{agentId}` | `{chatId, binding}` for a desktop draft |
| `LoadKeikiHistory` | `{chatId, limit?, before?}` | Conversation detail, `before`, and `pendingQuestions` |
| `WatchKeikiSidebar` | `{expandedAgents?, expandedGroups?}` | Snapshots with agents, groups, recent conversations, per-agent pages and group rosters |
| `WatchKeikiConversation` | `{chatId}` | Conversation detail and pending questions |
| `SetKeikiTakeover` | `{chatId, enabled}` | Existing Keiki takeover response |

Conversation limits default to 50, at most 100. History limits default to 50,
at most 500. Offsets and `before` are nonnegative integers; a null `nextOffset`
or `before` means there is no next page. Opening starts with the latest 50
messages; older history loads backwards with the returned `before`. History
messages keep their remote IDs and timestamps in the cache. Transcript entry
IDs have the stable form `keiki:<remote message id>`.

Bindings persist in the Local data directory and are unique per normalized
account email, organization, agent and conversation. Issuer and transient auth
revisions are not binding keys. Reopening from another group or window returns
the same local chat. Listing never creates chats. Drafts materialize remotely
on the first send. Existing local chats and historical account profiles are not
migrated or merged.

Use the existing durable chat send, steer and interrupt pipeline with the
returned `chatId`. Do not set `resume`: the engine resolves the binding.
Desktop conversations use the conversation streamed-steer endpoint. Contact
messages use the existing send endpoint and takeover contract. Dashboard
Copilot threads retain their existing execution and resume behavior.

Pending questions from bound streams have `readOnly: true` and
`replyInComposer: true`. A composer reply is a steer, not a dashboard-thread
answer/resume request. Stop drops the local stream and refreshes authoritative
history; it does not claim to cancel a remote turn where the API has no such
contract. Only authoritative history is imported as persisted conversation
content.

Polling matches Comet: sidebar snapshots every 15 seconds, retrying failures
after 10 seconds; conversation refreshes every 10 seconds idle or 3 seconds
while live/waiting. Sidebar watches load at most 20 recent conversations and
the first 50 conversations for each expanded agent, with at most 20 expanded
agents and 20 expanded groups. Use the paginated RPC for further rows.
Dropping a watch drops its polling. Account/organization changes invalidate
in-flight results and end bound transcript streams; sidebar watches emit a
reset and end. Reconnect in the new scope.
