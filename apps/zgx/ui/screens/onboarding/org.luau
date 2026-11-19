-- Choose an existing Keiki organization; account management stays on Keiki.
local ds = require("@ds")
local api = require("@account/api")
local actions = require("./actions")

export type Props = { step: number?, count: number?, on_back: () -> () }

return function(props: Props): Node
	local orgs = ui.use_state({} :: { api.Org })
	local loading = ui.use_state(false)
	local saving = ui.use_state(false)
	local failed = ui.use_state(nil :: string?)
	local selected = ui.use_state(nil :: api.Org?)
	local alive = true
	ui.on_cleanup(function() alive = false end)
	local function load()
		loading:set(true)
		failed:set(nil)
		api.orgs():after(function(list: { api.Org })
			if not alive then return end
			orgs:set(list)
			selected:set(nil)
			loading:set(false)
		end, function(error)
			if not alive then return end
			loading:set(false)
			failed:set(error.message)
		end)
	end
	ui.after(0, load)
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
					control = ui.show(function() return selected:get() == org end,
						ds.Icon { name = "check-circle", size = 16 }),
				}
			end),
			ds.Button { ui.t("onboarding.try_again"), disabled = loading, on_click = load },
		},
	}
end
