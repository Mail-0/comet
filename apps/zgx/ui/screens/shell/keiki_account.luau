local ds = require("@ds")
local auth = require("@account/auth")
local api = require("@account/api")
local M = {}
M.open = ui.state("keiki.account.open", false)
M.orgs = ui.state("keiki.account.orgs", {} :: any)
M.loading = ui.state("keiki.account.loading", false)
M.switching = ui.state("keiki.account.switching", false)
M.error = ui.state("keiki.account.error", nil :: string?)
M.selected = ui.state("keiki.account.selected", "")
local available = ui.shared("keiki.available", false)
local epoch = 0
local choice_epoch = 0
local last_scope = ""
function M.load()
	epoch += 1
	local mine = epoch
	M.loading:set(true); M.error:set(nil)
	api.orgs():after(function(orgs)
		if mine == epoch then M.orgs:set(orgs); M.loading:set(false) end
	end, function(error)
		if mine == epoch then M.loading:set(false); M.error:set(api.says(error.message)) end
	end)
end
function M.choose(id: string)
	if M.switching:peek() then return end
	choice_epoch += 1
	local mine = choice_epoch
	M.switching:set(true); M.error:set(nil)
	api.choose_org(id):after(function()
		if mine == choice_epoch then M.switching:set(false); M.open:set(false) end
	end, function(error)
		if mine == choice_epoch then
			M.switching:set(false); M.selected:set(auth.account:peek().organization_id or ""); M.error:set(api.says(error.message))
		end
	end)
end
ui.effect(function()
	local a = auth.account:get()
	if a.state ~= "signed_in" then M.open:set(false) end
	M.selected:set(a.organization_id or "")
	local wanted = available:get() and (M.open:get() or (a.state == "signed_in" and not a.organization_id))
	local scope = if wanted and a.user then `{a.user.id}/{a.organization_id or ""}` else ""
	if scope == last_scope then return end
	last_scope = scope
	if scope ~= "" then M.load() else epoch += 1; choice_epoch += 1; M.orgs:set({}); M.loading:set(false); M.switching:set(false); M.error:set(nil) end
end)
function M.picker(): Node
	return ui.column { w_full = true, gap = "$space.3",
		ds.Select {
			label = "Organization", value = M.selected,
			options = function()
				local rows = {}
				for _, org in M.orgs:get() do table.insert(rows, { value = org.organization_id or org.id, label = org.name }) end
				return rows
			end,
			disabled = function() return M.loading:get() or M.switching:get() or #M.orgs:get() == 0 end,
			on_change = M.choose,
		},
		ui.show(function() return M.loading:get() end, ui.text { "Loading organizations…", color = "$text.secondary" }),
		ui.show(function() return M.error:get() ~= nil end, ui.text { function() return M.error:get() or "" end, color = "$text.secondary" }),
		ui.show(function() return not M.loading:get() and #M.orgs:get() == 0 end,
			ui.text { "Create or join an organization on Keiki, then retry.", wrap = true, color = "$text.secondary" }),
		ds.Button { label = "Retry organizations", tone = "ghost", size = "sm", disabled = M.loading, on_click = M.load, ui.text { "Retry organizations" } },
	}
end
function M.menu(): Node
	return ds.Dialog { open = M.open, title = "Keiki account",
		body = function() local user = auth.user:get(); return if user then user.email else "Sign in to Keiki" end,
		cancel = "Done", build = function(): Node
			return ui.column { w_full = true, gap = "$space.3",
				ds.Surface { kind = "sunken", p = "$space.3", ui.row { items = "center", gap = "$space.3",
					ds.Surface { kind = "raised", w = 32, h = 32, rounded = "$radius.full", items = "center", justify = "center", ui.text { function() local user = auth.user:get(); return string.upper(string.sub(if user then user.name or user.email else "?", 1, 1)) end, text_size = "$font.size.lg", weight = "$font.weight.semibold" } },
					ui.text { function() local user = auth.user:get(); return if user then user.name or user.email else "Keiki account" end },
				} }, M.picker(),
			}
		end,
		aside = { label = "Sign out", on_click = function() M.open:set(false); auth.sign_out() end },
	}
end
return M
