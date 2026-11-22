local ds = require("@ds")
local keiki = require("@account/keiki")
local shared = require("@app/screens/chat/shared")
local function connection(value: any): any
	if type(value) ~= "table" then return nil end
	if value.mcpConnect then return value.mcpConnect end
	for _, child in value do local found = connection(child); if found then return found end end
	return nil
end

local function words(item: any): string
	local question = item.question or item
	return question.message or question.reason or "The agent is waiting for your input."
end
return function(chat: () -> string?, height: State<number>?): Node
	local questions = ui.computed(function(): { any }
		local id = chat()
		if not keiki.bound(id) then return {} end
		local detail = keiki.conversation:get()
		local rows = {}
		for _, question in if detail then detail.pendingQuestions or {} else {} do
			table.insert(rows, { key = `pending:{question.id}`, text = words(question), pending = true, connection = connection(question) })
		end
		for _, question in keiki.resolved:get()[id] or {} do
			table.insert(rows, { key = `resolved:{question.id}`, text = words(question), pending = false })
		end
		return rows
	end)
	ui.effect(function() if height and #questions:get() == 0 then height:set(0) end end)
	return ui.show(function() return #questions:get() > 0 end, ui.column {
		w_full = true, pb = "$space.2", on_resize = function(size)
			if height and math.abs(height:peek() - size.h) > 0.01 then height:set(size.h) end
		end,
		ui.each(questions, function(first: any, _i: number, now: State<any>): Node
		if not first.pending then return ui.row { ds.Badge { function() return `Resolved · {now:get().text}` end, icon = "check-circle", tone = "neutral", size = "sm", max_w = 460 } } end
		return ds.Surface { kind = "raised", p = "$space.3",
			ui.column { gap = "$space.2", w_full = true,
				ui.row { ds.Badge { "Pending question", tone = "warning", size = "sm" } },
				ui.text { function() return now:get().text end, wrap = true, weight = "$font.weight.medium" },
				ui.text { "Read-only · Reply in the composer to steer this conversation.", wrap = true, color = "$text.secondary", text_size = "$font.size.sm" },
				ds.Button { label = "Reply in composer", tone = "secondary", size = "sm", on_click = function() shared.focus:add(1) end, ui.text { "Reply in composer" } },
				ui.presence(function() return now:get().connection ~= nil end, function(present: boolean): Node
					if not present then return ui.div {} end
					local function spec(): any return now:get().connection or {} end
					local function status(): any return keiki.connections:get()[`{chat()}/{spec().presetId}`] or {} end
					return ui.column { gap = "$space.2", w_full = true,
						ds.Button { label = "Authorize connection", tone = "secondary", size = "sm", disabled = function() return status().status == "working" end,
							on_click = function() local id = chat(); if id then keiki.connect(id, spec().presetId) end end, ui.text { function() return `Authorize {spec().name or "connection"}` end } },
						ds.Button { label = "Check authorization", tone = "ghost", size = "sm", disabled = function() return status().status == "working" end,
							on_click = function() local id = chat(); if id then keiki.connect(id, spec().presetId, true) end end, ui.text { "Check authorization" } },
						ui.text { function() local s = status(); return s.error or (if s.status == "connected" or s.status == "connected_no_tools" then "Connected · reply in composer to continue" elseif s.opened then "Authorization opened in your browser" elseif s.status == "working" then "Checking connection…" else "") end, wrap = true, color = "$text.secondary" },
					}
				end),
			},
		}
		end, { key = function(r) return r.key end, id = "keiki-questions", w_full = true, gap = "$space.2" }),
	})
end
