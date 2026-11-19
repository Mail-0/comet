local ds = require("@ds")
local keiki = require("@account/keiki")
local shared = require("@app/screens/chat/shared")

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
			table.insert(rows, { key = `pending:{question.id}`, text = words(question), pending = true })
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
			},
		}
		end, { key = function(r) return r.key end, id = "keiki-questions", w_full = true, gap = "$space.2" }),
	})
end
