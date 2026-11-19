-- Account metadata comes only from the engine's Keiki facade.
local auth = require("@account/auth")
local api = require("@account/api")
local M = {}

export type Me = { name: string?, first_name: string?, email: string? }
export type Org = { organization_id: string, name: string }
M.me = ui.computed(function(): Me?
	local user = auth.user:get()
	return if user then { name = user.name, first_name = user.first_name, email = user.email } else nil
end)
M.orgs = ui.state("settings_orgs", {} :: { api.Org })
M.loading = ui.state("settings_orgs_loading", false)
M.failure = ui.state("settings_orgs_failure", nil :: string?)
M.org = ui.computed(function(): Org?
	local active = auth.account:get().organization_id
	for _, org in M.orgs:get() do
		if org.organization_id == active then return org end
	end
	return nil
end)
local generation = 0

function M.load_org()
	if M.loading:peek() or not auth.signed_in:peek() then return end
	generation += 1
	local mine = generation
	M.loading:set(true)
	M.orgs:set({})
	M.failure:set(nil)
	api.orgs():after(function(orgs: { api.Org })
		if mine ~= generation then return end
		M.orgs:set(orgs)
		M.loading:set(false)
	end, function(error)
		if mine ~= generation then return end
		M.loading:set(false)
		M.failure:set(error.message)
	end)
end

function M.forget()
	generation += 1
	M.orgs:set({})
	M.loading:set(false)
	M.failure:set(nil)
end

return M
