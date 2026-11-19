local state = require("./state")
local auth = require("@account/auth")
local api = require("@account/api")
local material = require("@ds/material")
local palettes = require("@ds/palettes")

local M = {}

function M.go(step: state.Step)
	state.step:set(step)
end

function M.next()
	local step = state.step:peek()
	M.finished(step)
	local past = false
	for _, current in state.order() do
		if past then state.step:set(current); return end
		past = current == step
	end
end

function M.back()
	local order = state.order()
	local i = table.find(order, state.step:peek()) or 1
	if i > 1 and order[i - 1] ~= "signin" then
		state.step:set(order[i - 1])
	end
end

local asking = false
function M.asking(): boolean
	return asking
end

function M.start_sign_in(kind: state.Account)
	state.account:set(kind)
	state.signin:set("waiting")
	asking = true
	auth.sign_in(kind, function(user, failure)
		asking = false
		if user then M.signed_in()
		elseif failure == "cancelled" then state.signin:set("ready")
		else state.signin:set("failed") end
	end)
end

function M.cancel_sign_in()
	auth.cancel()
	asking = false
	state.signin:set("ready")
end

function M.signed_in()
	state.path:set("signed_in")
	state.org_done:set(auth.account:peek().organization_id ~= nil)
	state.org_name:set("")
	state.signin:set("ready")
	asking = false
	M.start_run({})
	api.orgs():after(function(orgs: { api.Org })
		local active = auth.account:peek().organization_id
		for _, org in orgs do
			if org.organization_id == active then
				state.org_name:set(org.name)
				state.last_org:set(org.organization_id)
			end
		end
	end, function() end)
end

function M.start_run(_account_done: { string })
	local passed = if state.replay then {} else table.clone(state.device_done:peek())
	state.passed:set(passed)
	for _, step in state.order() do
		if step ~= "signin" and not passed[step] then
			state.step:set(step)
			return
		end
	end
	state.step:set("done")
end

function M.finished(step: state.Step)
	if state.PAGES[step] == "device" then
		local done = table.clone(state.device_done:peek())
		done[step] = true
		state.device_done:set(done)
	end
end

function M.work_in(org: api.Org, done: (() -> ())?, failed: ((any) -> ())?)
	api.choose_org(org.organization_id):after(function()
		state.last_org:set(org.organization_id)
		state.org_name:set(org.name)
		state.org_done:set(true)
		if done then done() end
	end, function(error)
		if failed then failed(error) end
	end)
end

function M.signed_out()
	state.wall_again:set(true)
	state.signin:set("ready")
	state.path:set("skipped")
	state.org_done:set(false)
	state.org_name:set("")
	state.step:set("signin")
end

function M.apply_scheme(mode: state.Appearance)
	if mode == "system" then
		ui.theme.follow_system { dark = palettes.scheme.dark, light = palettes.scheme.light }
		ui.invoke("appearance", ui.null)
	else
		ui.theme.follow_system(false)
		ui.theme.current:set(if mode == "dark" then palettes.scheme.dark else palettes.scheme.light)
		ui.invoke("appearance", mode == "dark")
	end
end

function M.appearance(mode: state.Appearance)
	state.appearance:set(mode)
	M.apply_scheme(mode)
end

M.frosted = material.frosted

function M.finish()
	if auth.signed_in:peek() then ui.invoke("app.open_main") end
end

return M
