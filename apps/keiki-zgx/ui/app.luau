--!strict

type Entry = { id: string, name: string? }
type Account = {
	signed_in: boolean,
	busy: boolean,
	orgs: { Entry }?,
	active_org: string?,
	agents: { Entry },
	agent_id: string?,
	reply: string,
	error: string?,
}
type Workspace = { status: string?, names: { string }? }

local account = ui.shared("keiki.account", { signed_in = false, busy = false, agents = {}, reply = "" } :: Account)
local workspace = ui.shared("keiki.workspace", {} :: Workspace)
local draft = ui.state("draft", "")
local orgs = ui.computed(function()
	return account:get().orgs or {}
end)
local agents = ui.computed(function()
	return account:get().agents
end)
local spaces = ui.computed(function()
	return workspace:get().names or {}
end)

local page = "#201b18"
local surface = "#2c2420"
local raised = "#40312a"
local accent = "#e9a47f"
local muted = "#c1aa9c"

local function button(label: string, action: () -> (), disabled: () -> boolean, id: string?)
	return ui.button({
		id = id,
		px = 12,
		py = 8,
		rounded = 7,
		bg = raised,
		hover = { bg = "#553b30" },
		disabled = disabled,
		on_click = action,
		ui.text({ label, literal = true, wrap = true }),
	})
end

local function busy()
	return account:get().busy
end

local function invoke_and_clear(action: string, value: string?)
	ui.invoke(action, value)
	draft:set("")
end

return ui.row({
	w_full = true,
	h_full = true,
	bg = page,
	color = "#fff5ef",
	ui.scroll({
		w = 270,
		shrink = 0,
		bg = surface,
		ui.column({
			p = 20,
			gap = 12,
			ui.text({ "Keiki", text_size = 25, bold = true, color = accent }),
			ui.text({ "Desktop preview", color = muted }),
			ui.text({ "Local workspace", bold = true }),
			ui.text({
				function()
					return workspace:get().status
				end,
				color = muted,
			}),
			ui.each(spaces, function(name)
				return ui.text({ name, literal = true, wrap = true })
			end),
			ui.text({ "Organizations", bold = true }),
			ui.each(orgs, function(org)
				return button(org.name or org.id, function()
					invoke_and_clear("choose_org", org.id)
				end, function()
					return busy() or account:get().active_org == org.id
				end)
			end, {
				key = function(org)
					return org.id
				end,
				gap = 6,
			}),
			ui.text({ "Keiki agents", bold = true }),
			ui.each(agents, function(agent)
				return button(agent.name or agent.id, function()
					invoke_and_clear("choose_agent", agent.id)
				end, function()
					return busy() or account:get().agent_id == agent.id
				end)
			end, {
				key = function(agent)
					return agent.id
				end,
				gap = 6,
			}),
		}),
	}),
	ui.column({
		grow = 1,
		min_w = 0,
		p = 24,
		gap = 16,
		ui.row({
			gap = 8,
			button("Sign in to Keiki", function()
				ui.invoke("sign_in")
			end, function()
				return busy() or account:get().signed_in
			end),
			button("Sign out", function()
				invoke_and_clear("sign_out")
			end, function()
				return busy() or not account:get().signed_in
			end),
		}),
		ui.text({
			function()
				local state = account:get()
				if state.error then
					return state.error
				end
				if state.busy then
					return "Working…"
				end
				if not state.signed_in then
					return "Sign in to get started."
				end
				return state.agent_id and "Ready" or "Choose a Keiki agent"
			end,
			color = muted,
			wrap = true,
		}),
		ui.scroll({
			grow = 1,
			min_h = 0,
			ui.text({
				function()
					return account:get().reply
				end,
				id = "reply",
				wrap = true,
			}),
		}),
		ui.input({
			id = "message",
			label = "Message",
			placeholder = "Message a Keiki agent",
			value = draft,
			bg = surface,
			px = 12,
			py = 10,
		}),
		button("Send", function()
			invoke_and_clear("send", draft:get())
		end, function()
			return busy() or account:get().agent_id == nil or draft:get() == ""
		end, "send"),
	}),
})
