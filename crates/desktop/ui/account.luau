--!strict
local kit = require("./kit")
type Entry = { id: string, name: string? }
type Account = {
	signed_in: boolean,
	busy: boolean,
	orgs: { Entry }?,
	active_org: string?,
	agents: { Entry },
	error: string?,
}
local state = ui.shared("keiki.account", { signed_in = false, busy = false, agents = {} } :: Account)
local orgs = ui.computed(function()
	return state:get().orgs or {}
end)
local conversations = ui.shared("keiki.conversations", {} :: { agent_id: string? })
local account = { state = state }

function account.sidebar()
	return {
		ui.text({ "Organizations", bold = true }),
		ui.each(orgs, function(org)
			return kit.list_row(org.name or org.id, function()
				ui.invoke("account.choose_org", org.id)
				ui.invoke("composer.clear")
			end, function()
				return state:get().busy or state:get().active_org == org.id
			end)
		end, {
			key = function(org)
				return org.id
			end,
			gap = 6,
		}),
	}
end

function account.header()
	return {
		ui.row({
			gap = 8,
			kit.button("Sign in to Keiki", function()
				ui.invoke("account.sign_in")
			end, function()
				return state:get().busy or state:get().signed_in
			end),
			kit.button("Sign out", function()
				ui.invoke("account.sign_out")
				ui.invoke("composer.clear")
			end, function()
				return state:get().busy or not state:get().signed_in
			end),
		}),
	}
end

function account.status()
	return {
		ui.text({
			function()
				local current = state:get()
				if current.error then
					return current.error
				end
				if current.busy then
					return "Working…"
				end
				if not current.signed_in then
					return "Sign in to get started."
				end
				return conversations:get().agent_id and "Ready" or "Choose a Keiki agent"
			end,
			color = kit.colors.muted,
			wrap = true,
		}),
	}
end

function account.modals(): { any }
	return {}
end
return account
