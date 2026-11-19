local ds = require("@ds")
local state = require("./state")
local actions = require("./actions")

export type Props = { on_back: () -> () }

return function(props: Props): Node
	local rows: { Node } = {}
	local org = state.org_name:peek()
	if org ~= "" then
		table.insert(rows, ds.Settings.Row {
			label = org, truncate = true,
			lead = ds.Tile { label = org, hue = "violet", size = 32 },
		})
	end
	table.insert(rows, ds.Settings.Row {
		label = ui.t("onboarding.done.local.label"),
		description = ui.t("onboarding.done.local.description"),
		lead = ds.Icon { name = "check-circle", size = 16 },
	})
	return ds.Onboarding.Step {
		back = ds.Onboarding.Back { on_click = props.on_back },
		continue = { label = ui.t("onboarding.done.open_zeron"), max_w = 260, on_click = actions.finish },
		ds.Onboarding.Form {
			wide = true,
			title = ui.t("onboarding.done.title"),
			body = ui.t("onboarding.done.body"),
			ds.Settings.Group(rows :: any),
		},
	}
end
