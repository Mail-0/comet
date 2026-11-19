local ds = require("@ds")
local keiki = require("@account/keiki")
local account = require("@app/screens/shell/keiki_account")
local opened = keiki.expanded
local Controls = require("@app/screens/shell/keiki_controls")
local Avatar = require("@app/screens/shell/keiki_avatar")
local ContextMenu = require("@ds/context_menu")

local rows = ui.computed(function(): { any }
	local snapshot = keiki.sidebar:get()
	if not snapshot then return { { key = "loading", text = "Loading your agents…", kind = "loading" } } end
	if snapshot.error or snapshot.reset then
		return { { key = "error", text = "Could not load Keiki. Check your connection and try again.", kind = "note" },
			{ key = "retry", text = "Retry Keiki", kind = "button", action = "retry" } }
	end
	local out, agents, grouped = {}, {}, {}
	local function note(key: string, text: string, depth: number?)
		table.insert(out, { key = key, text = text, kind = "note", depth = depth })
	end
	local function agent_row(agent: any, prefix: string, depth: number)
		local key = `{prefix}/agent/{agent.id}`
		local expanded = opened:get()[`agent:{agent.id}`] == true
		table.insert(out, { key = key, text = agent.name, kind = "button", depth = depth,
			icon = if expanded then "alt-arrow-down" else "alt-arrow-right",
			action = "agent", agent = agent.id })
		if not expanded then return end
		table.insert(out, { key = `{key}/new`, text = "New conversation", kind = "button", depth = depth + 1, icon = "plus", busy = true,
			action = "open", agent = agent.id })
		local page = keiki.pages:get()[agent.id]
		if not page or page.loading then note(`{key}/loading`, "Loading conversations…", depth + 1) end
		if page and page.error then
			note(`{key}/error`, page.error, depth + 1)
			table.insert(out, { key = `{key}/retry`, text = "Retry conversations", kind = "button", depth = depth + 1, action = "page", agent = agent.id })
		end
		for _, pinned in { true, false } do
		for _, conversation in if page then page.rows else {} do
			if (conversation.localPinned == true) ~= pinned or (conversation.localArchived and not keiki.show_archived:get()) then continue end
			local label = conversation.localTitle or conversation.contactName or conversation.lastMessage or conversation.phone
			if label == "" then label = conversation.phone end
			if pinned then label = `★ {label}` end
			if conversation.localArchived then label = `{label} · archived` end
			table.insert(out, { key = `{key}/conversation/{conversation.phone}`, text = label, kind = "button", depth = depth + 1, busy = true,
				action = "open", agent = agent.id, conversation = conversation.phone })
		end
		end
		if page and not page.loading and not page.error and #page.rows == 0 then note(`{key}/empty`, "No conversations yet", depth + 1) end
		if page and page.nextOffset ~= nil and not page.loading then
			table.insert(out, { key = `{key}/more`, text = "Load more conversations", kind = "button", depth = depth + 1, action = "more", agent = agent.id })
		end
	end
	for _, agent in snapshot.agents or {} do agents[agent.id] = agent end
	for _, group in snapshot.groups or {} do
		for _, member in group.members or {} do grouped[member.agentId] = true end
		local expanded = opened:get()[`group:{group.id}`] == true
		table.insert(out, { key = `group/{group.id}`, text = group.name, kind = "button", depth = 0,
			icon = if expanded then "alt-arrow-down" else "alt-arrow-right",
			action = "group", group = group.id })
		if expanded then
			local roster = keiki.rosters:get()[group.id]
			if roster and roster.loading then note(`group/{group.id}/loading`, "Loading group…", 1) end
			if roster and roster.error then
				table.insert(out, { key = `group/{group.id}/retry`, text = "Retry group", kind = "button", depth = 1, action = "roster", group = group.id })
			end
			for _, member in group.members or {} do
				if agents[member.agentId] then agent_row(agents[member.agentId], `group/{group.id}`, 1) end
			end
			if #(group.members or {}) == 0 then note(`group/{group.id}/empty`, "No agents in this group", 1) end
		end
	end
	table.insert(out, { key = "ungrouped", text = "Ungrouped", kind = "section" })
	local count = 0
	for _, agent in snapshot.agents or {} do
		if not grouped[agent.id] then count += 1; agent_row(agent, "ungrouped", 0) end
	end
	if count == 0 then note("ungrouped/empty", "No ungrouped agents") end
	if #(snapshot.agents or {}) == 0 then note("empty", "No agents yet. Create an agent on Keiki to get started.") end
	return out
end)

local function Row(first: any, _i: number, now: State<any>): Node
	local function indent(node: Node): Node
		for _ = 1, first.depth or 0 do node = ui.div { w_full = true, pl = "$space.4", node } end
		return node
	end
	if first.kind == "section" then return ui.text { first.text, weight = "$font.weight.semibold" } end
	if first.kind == "loading" then return ui.column { ds.Skeleton { w_full = true }, ui.text { first.text, color = "$text.secondary" } } end
	if first.kind == "note" then
		return indent(ui.text { function() return now:get().text end, wrap = true,
			text_size = "$font.size.sm", color = "$text.secondary" })
	end
	return indent(ContextMenu { items = function()
		local r = now:peek()
		if r.action == "open" and r.conversation then return { { label = "Conversation actions", on_select = function() Controls.conversation(r.agent, r.conversation) end } }
		elseif r.action == "agent" then return { { label = "Agent workspace", on_select = function() Controls.agent(r.agent, r.text) end } }
		elseif r.action == "group" then return { { label = "Group roster", on_select = function() Controls.group(r.group, r.text) end } } end
		return nil
	end, w_full = true,
	ui.row { w_full = true, items = "center",
		ds.Button { label = function() return now:get().text end, tone = "ghost", size = "sm", grow = 1, min_w = 0, justify = "start",
			disabled = function() return now:get().busy == true and keiki.opening:get() end,
			on_click = function()
				local r = now:peek()
				if r.action == "agent" then if keiki.toggle(`agent:{r.agent}`) then keiki.page(r.agent) end
				elseif r.action == "group" then if keiki.toggle(`group:{r.group}`) then keiki.roster(r.group) end
				elseif r.action == "open" then keiki.open(r.agent, r.conversation)
				elseif r.action == "page" or r.action == "more" then keiki.page(r.agent, r.action == "more")
				elseif r.action == "roster" then keiki.roster(r.group)
				else keiki.retry() end
			end,
			ui.div { w = 14, shrink = 0, ui.show(function() return now:get().icon ~= nil end, ds.Icon { name = function() return now:get().icon or "chat-round-line" end, size = 14 }) },
			if first.action == "agent" then Avatar(first.agent, first.text, 20) else nil,
			ui.text { function() return now:get().text end, truncate = true, grow = 1, min_w = 0 },
		},
		if first.action == "agent" or first.action == "group" or (first.action == "open" and first.conversation) then ds.Button {
			label = function() return (if first.action == "agent" then "Agent workspace: " elseif first.action == "group" then "Group roster: " else "Conversation actions: ") .. now:get().text end,
			tone = "ghost", size = "xs", on_click = function() local r = now:peek(); if r.action == "agent" then Controls.agent(r.agent, r.text) elseif r.action == "group" then Controls.group(r.group, r.text) else Controls.conversation(r.agent, r.conversation) end end,
			ui.text { "⋯" },
		} else nil,
		},
	})
end

return function(): Node
	return ui.show(function() return keiki.available:get() end, ui.column {
		id = "keiki-navigation", w_full = true, gap = "$space.2",
		ui.row { w_full = true, items = "center", justify = "between",
			ui.text { "Keiki", weight = "$font.weight.semibold" },
			ds.Button { label = "Create Keiki agent", tone = "ghost", size = "xs", on_click = Controls.create, ds.Icon { name = "plus", size = 16 } },
			ds.Button { label = "Keiki account", tone = "ghost", size = "xs", on_click = function() account.open:set(true) end, ds.Icon { name = "user-rounded", size = 16 } },
		},
		ui.scroll { w_full = true, max_h = 320, fade = { 0, 12 }, ui.each(rows, Row, { key = function(r) return r.key end, w_full = true }) },
		ui.show(function() return keiki.opening:get() end, ui.text { "Opening conversation…", color = "$text.secondary" }),
		ui.show(function() return keiki.failure:get() ~= nil end, ui.text { function() return keiki.failure:get() or "" end, wrap = true, color = "$text.secondary" }),
		ds.Button { label = function() return if keiki.show_archived:get() then "Hide archived conversations" else "Show archived conversations" end, tone = "ghost", size = "xs", on_click = function() keiki.show_archived:set(not keiki.show_archived:peek()) end, ui.text { function() return if keiki.show_archived:get() then "Hide archived conversations" else "Show archived conversations" end } },
		Controls.dialog(),
		ui.text { "Local chats", weight = "$font.weight.semibold" },
	})
end
