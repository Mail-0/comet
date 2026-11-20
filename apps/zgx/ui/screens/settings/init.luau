-- Settings' pages: General, Appearance, Notifications and Profile (yours,
-- your account included); Providers (this Mac's agents), Devices and
-- Projects (the folders your sessions work in); Members (your
-- organization); Cloud and Billing (the account's Cloud, while it has one).
-- Each is a module of its own with `ID`, `TITLE` and `view`, added to the
-- shell's settings (screens/shell/settings: the list, the page beside it),
-- ahead of Plugins and the shell's Archived. Also the search atop the list (⌘F,
-- search.luau), and the sessions' chimes (chimes.luau).
--
-- The shell preloads this module (screens/shell/init.luau's list), so a
-- broken save here shows as that failure, not a blank window.

local settings = require("@app/screens/shell/settings")
local route = require("@app/screens/shell/route")
local auth = require("@account/auth")
local search = require("@app/screens/settings/search")
local store = require("@app/screens/settings/store")
require("@app/screens/settings/chimes")

local general = require("@app/screens/settings/general")
local appearance = require("@app/screens/settings/appearance")
local activity_page = require("@app/screens/settings/activity")
local notifications = require("@app/screens/settings/notifications")
local profile = require("@app/screens/settings/profile")
local providers = require("@app/screens/settings/providers")
local devices = require("@app/screens/settings/devices")
local projects_page = require("@app/screens/settings/projects")
local files = require("@app/screens/settings/files")
local browser = require("@app/screens/settings/browser")
local browser_history = require("@app/screens/settings/browser_history")
local shortcuts = require("@app/screens/settings/shortcuts")
local voice = require("@app/screens/settings/voice")
local appshots = require("@app/screens/settings/appshots")
local about = require("@app/screens/settings/about")

type Module = { ID: string, TITLE: string, view: () -> Node }

-- Ahead of the pages others add (Plugins, which joins Organization after
-- Members; the shell's Archived): whichever of them is listed first.
local function ahead(): string
	for _, id in settings.pages:peek() do
		if id == "plugins" or id == "archived" then
			return id
		end
	end
	return "archived"
end

local function add(m: Module, icon: string, group: string, private: boolean?)
	settings.add {
		id = m.ID,
		title = m.TITLE,
		icon = icon,
		group = group,
		-- (The shell centres each page in the room beside the list.)
		view = m.view,
		before = ahead(),
		private = private,
	}
end

add(general, "settings-minimalistic", "Personal")
add(appearance, "tuning", "Personal")
add(activity_page, "inbox", "Personal")
add(notifications, "bell", "Personal")
add(voice, "microphone", "Personal")
add(files, "folder", "Personal")
add(shortcuts, "keyboard", "Personal")
add(appshots, "gallery", "Personal")
add(profile, "user-rounded", "Personal")
-- The panel's browser, under a heading of its own (room for what it grows:
-- site data, permissions, downloads).
add(browser, "global", "Browser")
add(browser_history, "clock-circle", "Browser")
add(providers, "widget", "Devices")
add(devices, "laptop", "Devices")
add(projects_page, "folder-with-files", "Devices")
-- Last, under Zeron's own heading (the shipping app's About and its
-- updates, in its app menu): after everything, Archived too.
settings.add {
	id = about.ID,
	title = about.TITLE,
	icon = "info-square",
	group = "Keiki",
	view = about.view,
}

-- The search: its field heads the list, and while something is typed its
-- results stand in the pages' place.
settings.head(search.Field, {
	replaces = function(): boolean
		return search.query:get() ~= ""
	end,
})

-- Account is Profile's now: a way in to `settings/account` (a link, a
-- history entry, a saved route) lands there, on the account's rows.
ui.effect(function()
	local r = route.current:get()
	if r.name == "settings" and r.page == "account" then
		search.open({ page = "profile", key = "email", title = "", where = "", score = 0 })
	end
end)

local asked_orgs = ui.state("settings_orgs_asked_for", "")
ui.effect(function()
	local user = auth.user:get()
	if user and asked_orgs:peek() ~= user.id then
		asked_orgs:set(user.id)
		store.load_org()
	elseif not user then
		asked_orgs:set("")
	end
end)

-- Signed out, nothing of the account stays for the next one.
ui.effect(function()
	if auth.state:get() == "signed_out" then
		store.forget()
	end
end)

return {}
