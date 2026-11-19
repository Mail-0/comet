-- The production gate/navigation and existing chat fixture, with engine replies.
local chat = require("./chat_fixture")
local ds = require("@ds")
local auth = require("@account/auth") :: any
local api = require("@account/api") :: any
local keiki = require("@account/keiki") :: any
local route = require("@app/screens/shell/route")
local store = require("@app/screens/chat/store") :: any
local account = require("@app/screens/shell/keiki_account")
local Gate = require("@app/screens/shell/keiki_gate")
local Sidebar = require("@app/screens/shell/keiki_sidebar")
local calls = ui.state("keiki.fixture.calls", "")
local fail = ui.state("keiki.fixture.fail", false)
local builds = ui.state("keiki.fixture.builds", 0)
local selected_org = "studio"
local action_info = { conversationId = "api:release", agentId = "nova", title = "Release plan", pinned = false, archived = false, peer = false, desktop = true }
local action_fail, templates_empty, roster_empty = false, false, false
local task = { id = "review", subagent = "Fern", status = "running", request = "Review the launch checklist" }
local snapshot = {
	agents = { { id = "nova", name = "Nova" }, { id = "fern", name = "Fern" } },
	groups = { { id = "team", name = "Design team", members = { { agentId = "nova", name = "Nova" } } } },
}
local function note(text: string) calls:append(text .. "\n") end
local function signed(org: string?)
	auth.account:set { state = "signed_in", env = "keiki", user = { id = "demo@keiki.test", email = "demo@keiki.test", name = "Demo" }, organization_id = org }
end
local function promise(answer: any, failed: boolean?): any
	return { await = function() if failed then error("offline") end; return answer end, after = function(_, ok, bad)
		ui.after(40, function() if failed then if bad then bad({ message = "offline" }) end else ok(answer) end end)
	end }
end
auth.sign_in = function() note("SignIn"); auth.account:set { state = "waiting", env = "keiki" } end
auth.cancel = function() auth.account:set { state = "signed_out", env = "keiki" } end
auth.sign_out = function() note("SignOut"); auth.cancel() end
api.orgs = function() note("ListOrgs"); return promise({ { id = "studio", organization_id = "studio", name = "Studio" }, { id = "research", organization_id = "research", name = "Research" } }) end
api.choose_org = function(id) note(`SelectOrg:{id}`); selected_org = id; signed(id); return promise(nil) end
keiki.watch_sidebar = function(params)
	if params == nil then keiki.sidebar:set(nil); return end
	ui.after(0, function() keiki.sidebar:set(snapshot) end)
end
keiki.watch_conversation = function(chat_id)
	keiki.conversation:set(if chat_id then { pendingQuestions = {} } else nil)
end
keiki.call = function(method, params)
	note(`{method}:{params.agentId or params.chatId or params.groupId or ""}:{params.offset or params.before or ""}`)
	if method == "GetKeikiAvatar" then return promise({}) end
	if action_fail then action_fail = false; return promise(nil, true) end
	if method == "KeikiConversationInfo" then return promise(action_info)
	elseif method == "KeikiConversationAction" then
		note(`action:{params.action}:{params.title or ""}`)
		if params.action == "rename" then action_info.title = params.title end
		if params.action == "pin" or params.action == "unpin" then action_info.pinned = params.action == "pin" end
		if params.action == "archive" or params.action == "restore" then action_info.archived = params.action == "archive" end
		return promise(if params.action == "deleteLocal" then { deletedLocal = true } else if params.action == "openDashboard" then { opened = true } else table.clone(action_info))
	elseif method == "ListKeikiTemplates" then return promise({ templates = if templates_empty then {} else { { id = "research", name = "Research partner", emoji = "✦", blurb = "A thoughtful collaborator" } } })
	elseif method == "CreateKeikiAgent" then note(`created:{params.template}:{params.name or ""}`); return promise({ ok = true, id = "new-agent", missing_secrets = {} })
	elseif method == "EndKeikiGroup" then note(`ended:{params.groupId}:{params.confirmed}`); return promise({ threadsEnded = 2, tasksCancelled = 1 })
	elseif method == "CancelKeikiTask" then note(`cancelled:{params.taskId}`); task.status = "cancelled"; keiki.conversation:set({ tasks = { table.clone(task) } }); return promise({ cancelled = true })
	elseif method == "SetKeikiTakeover" then note(`takeover:{params.enabled}`); keiki.conversation:set({ takeover = if params.enabled then {} else nil }); return promise({})
	elseif method == "KeikiPresetConnection" then note(`connection:{params.presetId}:{params.refresh}`); return promise({ status = if params.refresh then "connected" else "needs_auth", opened = not params.refresh }) end
	if method == "ListKeikiConversations" then
		if fail:peek() then fail:set(false); return promise(nil, true) end
		if params.offset == 50 then return promise({ conversations = { { phone = "api:older", contactName = "Earlier ideas" } } }) end
		return promise({ conversations = { { phone = "api:release", contactName = "Release plan", localTitle = action_info.title, localPinned = action_info.pinned, localArchived = action_info.archived }, { phone = "api:design", contactName = "Design review" } }, nextOffset = 50 })
	elseif method == "GetKeikiAgentGroupRoster" then return promise({ members = if roster_empty then {} else { { agentId = "nova", name = "Nova", role = "hub", status = "running", runningTurns = 1, pendingAsks = 2, backgroundTasks = 1, tokensIn = 1200, tokensOut = 300, costUsd = 0.04 } }, totals = { members = if roster_empty then 0 else 1, running = if roster_empty then 0 else 1, backgroundTasks = 1, costUsd = 0.04 } })
	elseif method == "OpenKeikiConversation" or method == "NewKeikiConversation" then return promise({ chatId = "keiki-bound-demo" })
	elseif method == "LoadKeikiHistory" then
		if params.before then
			local rows = { { id = "remote-old", role = "user", text = "Earlier imported history" } }
			for _, message in store.messages:peek() do table.insert(rows, message) end
			store.messages:set(rows)
			return promise({})
		end
		return promise({ before = 50 })
	end
	return promise({})
end
local base_open = store.open
store.open = function(id)
	if not keiki.bound(id) then base_open(id); return end
	base_open("s2")
	local session = table.clone(store.session:peek())
	base_open(id)
	session.id, session.title, session.state, session.harness = id, "Release plan", "ready", "copilot"
	store.session:set(session)
	store.messages:set({ { id = "remote-initial", role = "assistant", text = "Imported from your Keiki conversation." } })
end
local base_send = store.send
store.send = function(req)
	note(`{if store.session:peek().running then "Steer" else "Send"}:{req.session}:{req.text}`)
	return base_send(req)
end
local function command(id: string, run: () -> ()) ui.command { id = `keiki.{id}`, title = id, run = run } end
command("signed_in", function() signed(selected_org) end)
command("expired", auth.cancel)
command("needs_org", function() signed(nil) end)
command("loading", function() keiki.sidebar:set(nil) end)
command("opening", function() keiki.opening:set(true) end)
command("opened", function() keiki.opening:set(false) end)
command("error", function() keiki.sidebar:set({ error = "offline" }) end)
command("fail_page", function() fail:set(true) end)
command("empty", function() keiki.sidebar:set({ agents = {}, groups = {} }) end)
command("open", function() keiki.open("nova", "api:release") end)
command("action_fail", function() action_fail = true end)
command("templates_empty", function() templates_empty = true end)
command("roster_empty", function() roster_empty = true end)
command("tasks", function() keiki.conversation:set({ tasks = { table.clone(task) } }) end)
command("tasks_empty", function() keiki.conversation:set({ tasks = {} }) end)
command("contact", function() action_info.desktop = false; action_info.conversationId = "contact:operator" end)
command("peer", function() action_info.peer = true; action_info.desktop = false; action_info.conversationId = "agent:fern" end)
command("mcp", function() keiki.conversation:set({ pendingQuestions = { { id = "connect", question = { message = "Connect your team docs", mcpConnect = { agentId = "nova", presetId = "docs", name = "Team docs" } } } } }) end)
local function question(text: string)
	keiki.conversation:set({ pendingQuestions = { { id = "q1", question = { message = text }, readOnly = true, replyInComposer = true } } })
end
command("question", function() question("Which direction should we take?") end)
command("question_updated", function() question("Start with a useful example?") end)
command("resolved", function() keiki.conversation:set({ pendingQuestions = {} }) end)
command("stream", function()
	local session = table.clone(store.session:peek()); session.running = true; store.session:set(session)
	store.messages:push { id = "live", role = "assistant", text = "", streaming = true, started = ui.now() / 1000 }
	store.draft:set { id = "live", text = "A streamed Keiki reply" }
end)
command("scroll_up", function()
	local offset = require("@app/screens/chat/shared").offset
	offset:set(300); ui.after(40, function() offset:set(0) end)
end)
auth.account:set { state = "signed_out", env = "keiki" }
keiki.available:set(true)
return Gate(function()
	builds:add(1)
	return ui.row { w_full = true, h_full = true,
		ui.column { w = 290, h_full = true, p = "$space.3", Sidebar() },
		ui.div { grow = 1, min_w = 0, h_full = true, chat },
		account.menu(), ds.toast.Host {},
	}
end)
