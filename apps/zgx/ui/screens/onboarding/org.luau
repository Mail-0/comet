-- Choose an existing Keiki organization; account management stays on Keiki.
local ds = require("@ds")
local api = require("@account/api")
local actions = require("./actions")
local state = require("./state")

export type Props = { step: number?, count: number?, on_back: () -> () }

local orgs = ui.state("orgs", {} :: { api.Org })
local loading = ui.state("loading", false)
local saving = ui.state("saving", false)
local failed = ui.state("failed", nil :: string?)
local selected = ui.state("selected", nil :: api.Org?)

return function(props: Props): Node
	state.once_a_visit("keiki_org_ui", function()
		orgs:set({})
		loading:set(false)
		saving:set(false)
		failed:set(nil)
		selected:set(nil)
	end)
	local alive = true
	ui.on_cleanup(function() alive = false end)
	local function load()
		loading:set(true)
		failed:set(nil)
		api.orgs():after(function(list: { api.Org })
			if not alive then return end
			state.answer("keiki_orgs")
			orgs:set(list)
			selected:set(nil)
			loading:set(false)
		end, function(error)
			if not alive then return end
			state.answer("keiki_orgs")
			loading:set(false)
			failed:set(error.message)
		end)
	end
	if loading:peek() then
		load()
	else
		state.ask_until_answered("keiki_orgs", load)
	end
	return ds.Onboarding.Step {
		step = props.step, count = props.count,
		back = ds.Onboarding.Back { on_click = props.on_back },
		continue = {
			label = ui.t("common.continue"),
			disabled = function() return selected:get() == nil or saving:get() end,
			on_click = function()
				local org = selected:peek()
				if not org or saving:peek() then return end
				saving:set(true)
				actions.work_in(org, function()
					if not alive then return end
					saving:set(false)
					actions.go("done")
				end, function(error)
					if not alive then return end
					saving:set(false)
					failed:set(error.message)
				end)
			end,
		},
		ds.Onboarding.Form {
			title = "Choose your Keiki organization",
			body = function()
				if failed:get() then return api.says(failed:get()) end
				if loading:get() then return "Loading organizations…" end
				if #orgs:get() == 0 then return "Create or join an organization on Keiki, then try again." end
				return "Your chats and files stay on this computer."
			end,
			ds.Settings.Each(orgs, function(org: api.Org): Node
				return ds.Settings.Row {
					label = org.name,
					lead = ds.Tile { label = org.name, hue = "violet", size = 32 },
					on_click = function() selected:set(org) end,
					control = ui.show(function()
						local current = selected:get()
						return current ~= nil and current.id == org.id
					end,
						ds.Icon { name = "check-circle", size = 16 }),
				}
			end, { key = function(org: api.Org): string return org.id end }),
			ds.Button { ui.t("onboarding.try_again"), disabled = loading, on_click = load },
		},
	}
end
