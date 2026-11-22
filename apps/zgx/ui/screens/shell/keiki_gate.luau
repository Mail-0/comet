local ds = require("@ds")
local auth = require("@account/auth")
local account = require("@app/screens/shell/keiki_account")

return function(content: () -> Node): Node
	local main = ui.computed(function(): boolean return auth.signed_in:get() and auth.account:get().organization_id ~= nil end)
	local started = ui.state("keiki.gate.started", main:peek())
	local shell = if main:peek() then content() else nil
	ui.effect(function() if main:get() then started:set(true) end end)
	return ui.column { w_full = true, h_full = true,
		ui.presence(function() return started:get() end, function(ready: boolean)
			if not ready then return ui.div {} end
			return ui.show(function() return main:get() end, shell or content())
		end),
		ui.show(function() return not main:get() end, ui.presence(function()
		return if auth.signed_in:get() then "org" else "gate"
	end, function(kind: string)
		local choosing = kind == "org"
		local function sign_in_label(): string
			return if auth.state:get() == "waiting" then "Waiting for your browser…" elseif auth.state:get() == "restoring" then "Restoring your account…" else "Sign in to Keiki"
		end
		return ui.column { id = "keiki-gate", w_full = true, h_full = true,
			bg = "$background", items = "center", justify = "center", gap = "$space.6",
			ui.column { w = 380, gap = "$space.4",
				ds.Tile { label = "Keiki", hue = "violet", size = 48 },
				ui.text { if choosing then "Choose your organization" else "Welcome to Keiki", text_size = "$font.size.xl", weight = "$font.weight.semibold" },
				ui.text { "Your agents, conversations, and a little room to think. Your local chats stay on this computer.", wrap = true, color = "$text.secondary" },
				if choosing then account.picker() else ui.column { gap = "$space.3", w_full = true,
					ds.Button { label = sign_in_label,
						disabled = function() return auth.state:get() == "waiting" or auth.state:get() == "restoring" end,
						on_click = function() auth.sign_in("existing") end, ui.text { sign_in_label } },
					ui.show(function() return auth.state:get() == "waiting" end,
						ds.Button { label = "Cancel", tone = "ghost", on_click = auth.cancel, ui.text { "Cancel" } }),
					ui.text { function() local error = auth.failure:get(); return if error then auth.says(error) else "Continue securely in your browser." end, wrap = true, color = "$text.secondary" },
				},
			},
		}
	end)) }
end
