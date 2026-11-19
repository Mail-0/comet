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
local snapshot = {
	agents = { { id = "nova", name = "Nova" }, { id = "fern", name = "Fern" } },
	groups = { { id = "team", name = "Design team", members = { { agentId = "nova", name = "Nova" } } } },
}
local function note(text: string) calls:append(text .. "\n") end
local function signed(org: string?)
	auth.account:set { state = "signed_in", env = "keiki", user = { id = "demo@keiki.test", email = "demo@keiki.test", name = "Demo" }, organization_id = org }
end
local function promise(answer: any, error: boolean?): any
	return { after = function(_, ok, bad)
		ui.after(40, function() if error then if bad then bad({ message = "offline" }) end else ok(answer) end end)
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
	if method == "ListKeikiConversations" then
		if fail:peek() then fail:set(false); return promise(nil, true) end
		if params.offset == 50 then return promise({ conversations = { { phone = "api:older", contactName = "Earlier ideas" } } }) end
		return promise({ conversations = { { phone = "api:release", contactName = "Release plan" }, { phone = "api:design", contactName = "Design review" } }, nextOffset = 50 })
	elseif method == "GetKeikiAgentGroupRoster" then return promise({ members = snapshot.groups[1].members })
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
