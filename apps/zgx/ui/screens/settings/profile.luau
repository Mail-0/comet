-- Keiki account identity and organization; workspace data always stays local.
local ds = require("@ds")
local host = require("@app/screens/settings/host")
local auth = require("@account/auth")
local api = require("@account/api")
local store = require("@app/screens/settings/store")
local search = require("@app/screens/settings/search")
local visits = require("@app/screens/settings/visits")
local route = require("@app/screens/shell/route")
local toast = require("@ds/toast")

local M = { ID = "profile", TITLE = "Account" }
search.index(M.ID, ui.t("settings.profile.title"), "Keiki account organization sign in sign out", {
	{ key = "email", title = ui.t("settings.profile.email.label"), terms = "Keiki account email" },
	{ key = "organization", title = "Organization", terms = "Keiki organization" },
	{ key = "sign_out", title = ui.t("settings.profile.sign_out.label"), terms = "logout sign out" },
})
local confirming = ui.state("settings_sign_out_confirm", false)
local leaving = ui.state("settings_signing_out", false)

local function sign_out()
	if leaving:peek() then return end
	leaving:set(true)
	auth.sign_out(function()
		store.forget()
		leaving:set(false)
		host.invoke("app.onboard")
	end)
end

function M.view(): Node
	ui.effect(function()
		local current = route.current:get()
		if current.name == "settings" and current.page == M.ID and auth.signed_in:get() then
			visits.once("profile", store.load_org)
		end
	end)
	local choice = ui.use_state(auth.account:peek().organization_id or "")
	local switching = ui.use_state(false)
	ui.effect(function() choice:set(auth.account:get().organization_id or "") end)
	return ui.column {
		w_full = true,
		ds.Settings.Page {
			title = ui.t("settings.profile.title"),
			description = "Your Keiki account. This app always uses the local workspace.",
			ui.show(function() return auth.signed_in:get() end, ds.Settings.Section {
				ds.Settings.Group {
					search.anchor("email", ds.Settings.Row {
						label = function() local user = auth.user:get(); return if user then user.email else "Not signed in" end,
						description = function() local user = auth.user:get(); return if user then user.name or user.first_name or "Keiki" else "Sign in to Keiki to use Copilot." end,
						private_label = true, icon = "user-rounded",
					}),
					search.anchor("organization", ds.Settings.Row {
						label = "Organization",
						description = function() return if store.failure:get() then api.says(store.failure:get()) else "Changing organizations does not move your local data." end,
						control = ds.Select {
							value = choice,
							options = function(): { any }
								local out = {}
								for _, org in store.orgs:get() do table.insert(out, { value = org.organization_id, label = org.name }) end
								return out
							end,
							disabled = function() return store.loading:get() or switching:get() or #store.orgs:get() == 0 end,
							on_change = function(id: string)
								if switching:peek() then return end
								switching:set(true)
								api.choose_org(id):after(function() switching:set(false) end, function(error)
									switching:set(false)
									choice:set(auth.account:peek().organization_id or "")
									toast.show { title = "Could not change organization", body = api.says(error.message), tone = "danger" }
								end)
							end,
						},
					}),
					search.anchor("sign_out", ds.Settings.Row {
						label = ui.t("settings.profile.sign_out.label"),
						description = "Local workspace data remains on this computer.",
						danger = true, disabled = leaving,
						on_click = function() confirming:set(true) end,
					}),
				},
			}),
			ui.show(function() return not auth.signed_in:get() end, ds.Settings.Section {
				ds.Settings.Group {
					ds.Settings.Row {
						label = "Keiki",
						description = function() return if auth.state:get() == "restoring" then "Restoring your Keiki account…" else "Sign in to Keiki to use Copilot. Your workspace stays local." end,
						control = ds.Button { label = "Sign in", disabled = function() return auth.state:get() == "restoring" end,
							on_click = function() host.invoke("app.onboard") end },
					},
				},
			}),
		},
		ds.Dialog {
			open = confirming,
			title = ui.t("settings.profile.sign_out.confirm_title"),
			body = "Sign out of Keiki? Your local chats and files will stay on this computer.",
			tone = "danger",
			confirm = { label = ui.t("settings.profile.sign_out.label"), on_click = sign_out },
		},
	}
end

return M
