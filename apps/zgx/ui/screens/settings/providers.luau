-- Copilot is the local engine's only production harness.
local ds = require("@ds")
local agents = require("@account/agents")
local auth = require("@account/auth")
local visits = require("@app/screens/settings/visits")
local search = require("@app/screens/settings/search")
local M = { ID = "providers", TITLE = "Providers" }
search.index(M.ID, ui.t("settings.providers.title"), "Keiki Copilot agent", {
	{ key = "agents", title = "Copilot", terms = "Keiki account agent" },
})
local function copilot(): agents.Agent?
	for _, agent in agents.now:get().list do if agent.id == "copilot" then return agent end end
	return nil
end
function M.view(): Node
	visits.once("providers", function() agents.target(nil); agents.load() end)
	return ds.Settings.Page {
		title = ui.t("settings.providers.title"),
		description = "Copilot uses your Keiki account. Manage sign-in under Account.",
		ds.Settings.Section {
			ds.Settings.Group {
				search.anchor("agents", ds.Settings.Row {
					label = "Copilot",
					description = function()
						local agent = copilot()
						if agent and not agent.enabled then return "Disabled on this device." end
						return if auth.signed_in:get() then "Uses your signed-in Keiki account." else "Sign in to Keiki under Account."
					end,
					control = ui.show(function(): boolean local agent = copilot(); return agent ~= nil and not agent.enabled end,
						ds.Button { label = "Enable", on_click = function() agents.set_enabled("copilot", true) end }),
				}),
			},
		},
	}
end
return M
