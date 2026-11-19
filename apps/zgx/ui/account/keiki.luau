-- The engine owns credentials and bindings; these are display data and opaque chat IDs.
local auth = require("@account/auth")
local route = require("@app/screens/shell/route")
local M = {}
M.available = ui.shared("keiki.available", false)
M.sidebar = ui.shared("keiki.sidebar", nil :: any)
M.conversation = ui.shared("keiki.conversation", nil :: any)
M.pages = ui.state("keiki.pages", {} :: any)
M.rosters = ui.state("keiki.rosters", {} :: any)
M.opening = ui.state("keiki.opening", false)
M.failure = ui.state("keiki.failure", nil :: string?)
M.history = ui.state("keiki.history", {} :: any)
M.resolved = ui.state("keiki.resolved", {} :: any)
M.expanded = ui.state("keiki.expanded", {} :: any)
local generation = 0
local scope = ""
local pending: { [string]: any } = {}
local watched: string? = nil
function M.watch_sidebar(params: any?)
	ui.invoke("keiki.sidebar", params)
end
function M.watch_conversation(chat: string?)
	ui.invoke("keiki.conversation", chat)
end

function M.call(method: string, params: any?): any
	return ui.call("keiki.call", { method = method, params = params or {} })
end
function M.bound(id: string?): boolean
	return id ~= nil and string.sub(id, 1, 12) == "keiki-bound-"
end
function M.retry()
	M.failure:set(nil)
	M.sidebar:set(nil)
	M.follow()
end
function M.follow()
	local agents, groups = {}, {}
	for key, yes in M.expanded:peek() do
		if yes and string.sub(key, 1, 6) == "agent:" then table.insert(agents, string.sub(key, 7)) end
		if yes and string.sub(key, 1, 6) == "group:" then table.insert(groups, string.sub(key, 7)) end
	end
	M.watch_sidebar({ expandedAgents = agents, expandedGroups = groups })
end
function M.toggle(key: string): boolean
	local yes = not M.expanded:peek()[key]
	local count = 0
	for other, expanded in M.expanded:peek() do
		if expanded and string.sub(other, 1, 6) == string.sub(key, 1, 6) then count += 1 end
	end
	if yes and count >= 20 then M.failure:set("Collapse another item before expanding more."); return false end
	M.expanded:set_key(key, yes)
	M.follow()
	return yes
end
function M.page(agent: string, more: boolean?)
	local old = M.pages:peek()[agent]
	if old and old.loading then return end
	local offset = if more and old then old.nextOffset else 0
	if more and offset == nil then return end
	local mine = generation
	M.pages:set_key(agent, { rows = if old then old.rows else {}, loading = true, nextOffset = offset })
	M.call("ListKeikiConversations", { agentId = agent, limit = 50, offset = offset }):after(function(answer)
		if mine ~= generation then return end
		local rows = if more and old then table.clone(old.rows) else {}
		local seen = {}
		for _, row in rows do seen[row.phone] = true end
		for _, row in answer.conversations or {} do
			if not seen[row.phone] then table.insert(rows, row); seen[row.phone] = true end
		end
		M.pages:set_key(agent, { rows = rows, nextOffset = answer.nextOffset, loading = false })
	end, function(error)
		if mine ~= generation then return end
		M.pages:set_key(agent, { rows = if old then old.rows else {}, nextOffset = offset, error = error.message })
	end)
end
function M.roster(group: string)
	local mine = generation
	M.rosters:set_key(group, { loading = true })
	M.call("GetKeikiAgentGroupRoster", { groupId = group }):after(function(answer)
		if mine == generation then M.rosters:set_key(group, answer) end
	end, function(error)
		if mine == generation then M.rosters:set_key(group, { error = error.message }) end
	end)
end
function M.open(agent: string, identity: string?)
	if M.opening:peek() then return end
	local mine = generation
	M.opening:set(true); M.failure:set(nil)
	M.call(if identity then "OpenKeikiConversation" else "NewKeikiConversation", { agentId = agent, conversationId = identity }):after(function(answer)
		if mine ~= generation then return end
		M.opening:set(false)
		route.open_session(answer.chatId)
	end, function(error)
		if mine ~= generation then return end
		M.opening:set(false); M.failure:set(error.message)
	end)
end
function M.older(chat: string, initial: boolean?)
	local old = M.history:peek()[chat]
	if old and old.loading then return end
	if not initial and (not old or old.before == nil) then return end
	local mine = generation
	M.history:set_key(chat, { loading = true, before = if old then old.before else nil })
	M.call("LoadKeikiHistory", { chatId = chat, limit = 50, before = if not initial and old then old.before else nil }):after(function(answer)
		if mine == generation then M.history:set_key(chat, { before = answer.before, loading = false }) end
	end, function(error)
		if mine == generation then M.history:set_key(chat, { before = if old then old.before else nil, error = error.message }) end
	end)
end
function M.retry_conversation()
	local now = route.current:peek()
	if M.bound(now.session) then M.watch_conversation(now.session) end
end

ui.effect(function()
	local a = auth.account:get()
	local next_scope = if M.available:get() and a.state == "signed_in" and a.organization_id then `{a.user.id}/{a.organization_id}` else ""
	if next_scope == scope then return end
	generation += 1
	local previous = scope
	scope = next_scope
	M.pages:set({}); M.rosters:set({}); M.history:set({}); M.resolved:set({}); M.expanded:set({}); pending = {}
	M.sidebar:set(nil)
	M.failure:set(nil); M.opening:set(false); watched = nil
	M.watch_conversation(nil)
	M.watch_sidebar(if scope ~= "" then {} else nil)
	if previous ~= "" and M.bound(route.current:peek().session) then route.new_chat() end
end)
ui.effect(function()
	local now = route.current:get()
	local a = auth.account:get()
	local chat = if M.available:get() and a.state == "signed_in" and a.organization_id and M.bound(now.session) then now.session else nil
	if chat == watched then return end
	watched = chat
	if M.available:peek() then M.watch_conversation(chat) end
	if chat then M.older(chat, true) end
end)
ui.effect(function()
	local detail = M.conversation:get()
	local chat = route.current:peek().session
	if not detail or detail.error or not M.bound(chat) then return end
	local current = {}
	for _, q in detail.pendingQuestions or {} do current[q.id] = q end
	local done, seen = {}, {}
	for _, q in M.resolved:peek()[chat] or {} do
		if not current[q.id] and not seen[q.id] then table.insert(done, q); seen[q.id] = true end
	end
	for id, q in pending[chat] or {} do
		if not current[id] and not seen[id] then table.insert(done, q); seen[id] = true end
	end
	pending[chat] = current
	M.resolved:set_key(chat, done)
end)
ui.effect(function()
	local snapshot = M.sidebar:get()
	if not snapshot or snapshot.error then return end
	for _, item in snapshot.rosters or {} do M.rosters:set_key(item.groupId, item.roster) end
	for _, item in snapshot.pages or {} do
		local old = M.pages:peek()[item.agentId]
		if old and not old.loading then
			local rows, seen = {}, {}
			for _, row in item.page.conversations or {} do table.insert(rows, row); seen[row.phone] = true end
			for _, row in old.rows do if not seen[row.phone] then table.insert(rows, row) end end
			local next_offset = if #rows > 50 then old.nextOffset elseif #(item.page.conversations or {}) == 50 then 50 else nil
			M.pages:set_key(item.agentId, { rows = rows, nextOffset = next_offset })
		end
	end
end)
return M
