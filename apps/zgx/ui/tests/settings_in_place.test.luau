-- Local settings and Keiki account identity in the mounted shell.

-- A node's identity under `scope`, by its kind and label: `@id`.
local function node(app: any, scope: string, kind: string, label: string): string?
	for line in app:tree(scope):gmatch("[^\n]+") do
		if line:find(` {kind} "{label}"`, 1, true) then
			return line:match("@(%d+)")
		end
	end
	return nil
end

-- Whether `text` shows anywhere under `scope`.
local function shows(app: any, scope: string, text: string): boolean
	return app:tree(scope):find(text, 1, true) ~= nil
end

local function calls(app: any): { string }
	return (app:state("settings_stub_calls") :: any) or {}
end

local function asked(app: any, entry: string): boolean
	for _, c in calls(app) do
		if c == entry then
			return true
		end
	end
	return false
end

local PAGE = "#settings-page"
local NAV = "#settings-nav"

local function open(page: string?): any
	local app = mount("shell_fixture.luau")
	app:resize(1200, 900)
	app:frames(30)
	app:command("settings_stub.sign_in")
	app:command("shell.settings")
	app:frames(20)
	if page then
		app:click(`{NAV} >> role:tab "{page}"`)
		app:frames(30)
	end
	return app
end

test("Providers offers Copilot without retired agents or installers", function()
	local app = open("Providers")
	expect(shows(app, PAGE, "Copilot")).to_be(true)
	expect(shows(app, PAGE, "Mock")).to_be(false)
	expect(shows(app, PAGE, "Add an agent")).to_be(false)
end)

test("the settings pages keep local tools and the Keiki account", function()
	local app = open()
	for _, label in { "Personal", "Devices", "Keiki", "General", "Appearance", "Notifications", "Account", "Providers", "Projects" } do
		expect(app:has(`{NAV} >> "{label}"`)).to_be(true)
	end
	for _, label in { "Members", "Cloud", "Billing", "Signing keys" } do
		expect(app:has(`{NAV} >> role:tab "{label}"`)).to_be(false)
	end
	expect(app:state("shell_route_path")).to_be("/settings/general")
	expect(shows(app, PAGE, "Credit Keiki in commits")).to_be(false)
end)

test("the search finds a page and a setting, and opens the setting's page at it", function()
	local app = open()
	app:type(`{NAV} >> #settings-search`, "task completed")
	app:frames(10)
	-- The results stand in the pages' place.
	expect(app:has(`{NAV} >> "General"`)).to_be(false)
	expect(app:has(`#settings-search-results >> "Task completed"`)).to_be(true)
	expect(shows(app, "#settings-search-results", "Notifications · Sounds")).to_be(true)
	app:key("Enter")
	app:frames(30)
	expect(app:state("shell_route_path")).to_be("/settings/notifications")
	expect(app:state("settings_search_lit")).to_be("sound_done")
	-- A word that misses nothing else finds the page itself.
	app:fill(`{NAV} >> "Search settings"`, "providers")
	app:frames(10)
	expect(app:has(`#settings-search-results >> "Providers"`)).to_be(true)
	app:fill(`{NAV} >> "Search settings"`, "zzzz")
	app:frames(10)
	expect(app:has(`#settings-search-results >> "No settings match."`)).to_be(true)
	-- Cleared, the pages come back.
	app:fill(`{NAV} >> "Search settings"`, "")
	app:frames(10)
	expect(app:has(`{NAV} >> "General"`)).to_be(true)
end)






test("commit credit has no setting or engine requests", function()
	local app = open()
	expect(app:has(`{PAGE} >> role:switch "Credit Keiki in commits"`)).to_be(false)
	expect(asked(app, "settings.engine:GetCommitCredit")).to_be(false)
	expect(asked(app, "settings.engine:SetCommitCredit")).to_be(false)
	app:fill(`{NAV} >> #settings-search`, "co-author")
	app:frames(10)
	expect(app:has(`#settings-search-results >> "No settings match."`)).to_be(true)
end)

test("the Keiki organization list loads once per account page visit", function()
	local app = open()
	local function requested(): number
		local count = 0
		for _, call in calls(app) do if call == "account.orgs" then count += 1 end end
		return count
	end
	local before = requested()
	app:click(`{NAV} >> role:tab "Account"`)
	app:frames(30)
	expect(requested()).to_be(before + 1)
	app:frames(30)
	expect(requested()).to_be(before + 1)
	app:click(`{NAV} >> role:tab "General"`)
	app:frames(20)
	app:click(`{NAV} >> role:tab "Account"`)
	app:frames(30)
	expect(requested()).to_be(before + 2)
end)

test("signing out asks first, then ends at setup", function()
	local app = open("Account")
	app:scroll(PAGE, 3000)
	app:frames(4)
	app:click(`{PAGE} >> role:button "Sign out"`)
	app:frames(20)
	app:click('role:dialog "Sign out of Keiki?" >> role:button "Sign out"')
	app:frames(20)
	expect(asked(app, "account.sign_out")).to_be(true)
	expect(asked(app, "app.onboard")).to_be(true)
end)

test("the local settings headings remain distinct and ordered", function()
	local app = open()
	local tree = app:tree(NAV)
	local function at(label: string): number return (tree:find(`"{label}"`, 1, true)) or 0 end
	for _, label in { "Personal", "Devices", "Keiki" } do
		local count = 0
		for line in tree:gmatch("[^\n]+") do
			if line:find(` text "{label}"`, 1, true) then count += 1 end
		end
		expect(count).to_be(1)
	end
	expect(at("Personal") < at("General")).to_be(true)
	expect(at("Account") < at("Devices")).to_be(true)
	expect(at("Providers") < at("Keiki")).to_be(true)
end)


-- A node's box under `scope`: x, y, w, h.
local function box(app: any, target: string): (number, number, number, number)
	local tree: string = app:tree(target)
	local line = string.match(tree, "[^\n]+") or ""
	local x, y, w, h = line:match(" (%-?%d+),(%-?%d+) (%d+)x(%d+)")
	return tonumber(x) or 0, tonumber(y) or 0, tonumber(w) or 0, tonumber(h) or 0
end


test("a way in to the old Account page lands on Profile, at the account's rows", function()
	local app = open("General")
	app:command("settings_stub.open_account")
	app:frames(30)
	expect(app:state("shell_route_path")).to_be("/settings/profile")
	expect(app:state("settings_search_lit")).to_be("email")
	expect(shows(app, PAGE, "ada@example.net")).to_be(true)
end)

test("an accent chosen in Appearance is the one the window wears, kept as chosen", function()
	local app = open("Appearance")
	expect(app:has(`{PAGE} >> role:radio "Theme default"`)).to_be(true)
	app:click(`{PAGE} >> role:radio "Orange"`)
	app:frames(10)
	-- The setting the shell's `theme` reads (ds.look), and the swatch on.
	expect(app:state("accent")).to_be("orange")
	expect(app:tree(`{PAGE} >> role:radio "Orange"`)).to_contain("checked")
	expect(app:tree(`{PAGE} >> role:radio "Theme default"`)).to_contain("unchecked")
	expect(shows(app, PAGE, "Orange")).to_be(true)
	-- Back to the theme's own.
	app:click(`{PAGE} >> role:radio "Theme default"`)
	app:frames(10)
	expect(app:state("accent")).to_be("default")
end)

test("Devices lists your computers, this one first, and renames one", function()
	local app = open("Devices")
	expect(shows(app, PAGE, "MacBook Pro")).to_be(true)
	expect(shows(app, PAGE, "devbox")).to_be(true)
	expect(shows(app, PAGE, "This device")).to_be(true)
	local mac = node(app, PAGE, "text", "MacBook Pro")
	app:click(`{PAGE} >> role:button "More for devbox"`)
	app:frames(10)
	app:click('role:menu >> role:button "Rename"')
	app:frames(10)
	app:fill('"Name for devbox"', "build-box")
	app:key("Enter")
	app:frames(10)
	expect(asked(app, "sessions.rename_device:box:build-box")).to_be(true)
	expect(node(app, PAGE, "text", "MacBook Pro")).to_be(mac)
end)

test("Files: the autosave delay shows while Autosave is on, each kept as set", function()
	local app = open("Files")
	expect(shows(app, PAGE, "Word wrap")).to_be(true)
	expect(shows(app, PAGE, "Show hidden and ignored files")).to_be(true)
	expect(shows(app, PAGE, "Autosave delay")).to_be(false)
	app:click(`{PAGE} >> role:switch "Autosave"`)
	app:frames(10)
	expect(app:state("settings_files_autosave")).to_be(true)
	expect(shows(app, PAGE, "Autosave delay")).to_be(true)
	app:click(`{PAGE} >> role:switch "Word wrap"`)
	app:frames(10)
	expect(app:state("settings_files_word_wrap")).to_be(true)
end)



test("Notifications says when this computer's banner settings can't be read, and holds its switches", function()
	local app = open("General")
	app:command("settings_stub.refuse_next")
	app:click(`{NAV} >> role:tab "Notifications"`)
	app:frames(20)
	expect(shows(app, PAGE, "Couldn't read this computer's notification settings")).to_be(true)
	app:click(`{PAGE} >> role:switch "Desktop notifications"`)
	app:frames(10)
	expect(asked(app, "notifications.set")).to_be(false)
	app:click(`{PAGE} >> role:button "Try again"`)
	app:frames(20)
	expect(shows(app, PAGE, "Couldn't read this computer's notification settings")).to_be(false)
end)

test("Appearance's code font size is kept as chosen, for the editor to read", function()
	local app = open("Appearance")
	expect(app:state("settings_code_font_size")).to_be(13)
	-- (Below the page's fold: scrolled to first.)
	app:scroll(PAGE, 200)
	app:frames(10)
	app:click(`{PAGE} >> role:button "Code & diff font size"`)
	app:frames(10)
	app:click('role:menuitem "15 pt"')
	app:frames(10)
	expect(app:state("settings_code_font_size")).to_be(15)
end)

test("Shortcuts lists the shell's shortcuts under their groups, each with its keys", function()
	local app = open("Shortcuts")
	-- (Until the shell's table is in, the page says so.)
	if shows(app, PAGE, "No shortcuts yet") then
		return
	end
	for _, title in { "New chat", "Search", "Settings" } do
		expect(shows(app, PAGE, title)).to_be(true)
	end
	expect(shows(app, PAGE, "Sessions")).to_be(true)
end)

test("Appearance's conversation width is a slider kept as set, within production's range", function()
	local app = open("Appearance")
	expect(app:state("settings_conversation_width")).to_be(660)
	local slider = `{PAGE} >> role:slider ~"Conversation width"`
	expect(app:has(slider)).to_be(true)
	-- (Under the theme and motion sections: brought into view.)
	app:scroll(PAGE, 600)
	app:frames(4)
	app:click(slider)
	app:key("End")
	app:frames(10)
	expect(app:state("settings_conversation_width")).to_be(1200)
	app:key("Home")
	app:frames(10)
	expect(app:state("settings_conversation_width")).to_be(560)
end)

test("the interface text size scales the window's text and lines, at once and back", function()
	local app = open("Appearance")
	local function height(): number
		local _, _, _, h = box(app, `{PAGE} >> "Accent color"`)
		return h
	end
	expect(app:state("text_size")).to_be(13)
	local before = height()
	expect(before).to_be(18)
	app:set_state("text_size", 16)
	app:frames(20)
	expect(height()).to_be(22)
	app:set_state("text_size", 13)
	app:frames(20)
	expect(height()).to_be(before)
end)

test("a new chat's picture: chosen, kept, named, removed; its effect said, and gone with it", function()
	local app = open("Appearance")
	-- (Tall enough that the New chat section shows whole.)
	app:resize(1200, 1800)
	app:set_state("shell_wallpaper", "none")
	app:set_state("shell_wallpaper_effect", "halftone")
	app:frames(6)
	-- Nothing to draw over: no effect to choose.
	expect(shows(app, PAGE, "colored print dots")).to_be(false)
	app:set_state("shell_wallpaper", "image")
	app:frames(6)
	expect(shows(app, PAGE, "No image chosen")).to_be(true)
	app:click(`{PAGE} >> role:button "Choose Image"`)
	app:frames(10)
	-- Kept by the host (its copy), then shown by its name.
	expect(asked(app, "wallpaper.keep:assets/favicons/zeron.png")).to_be(true)
	expect(shows(app, PAGE, "zeron.png")).to_be(true)
	expect(shows(app, PAGE, "colored print dots")).to_be(true)
	expect(app:has(`{PAGE} >> role:button "Adjust"`)).to_be(true)
	expect(app:has(`{PAGE} >> role:button "Replace"`)).to_be(true)
	local image = app:state("shell_wallpaper_image") :: any
	expect(image and image.zoom).to_be(1)
	app:click(`{PAGE} >> role:button "Remove"`)
	app:frames(10)
	expect(app:state("shell_wallpaper")).to_be("none")
	expect(app:state("shell_wallpaper_image")).to_be_nil()
	expect(asked(app, "wallpaper.forget:assets/favicons/zeron.png")).to_be(true)
	expect(shows(app, PAGE, "colored print dots")).to_be(false)
end)

-- About & updates: the host's `about` stood in for, each state of an update.
local ABOUT = "About & updates"
local function about(app: any, fields: { [string]: any })
	local now = { app_version = "0.2.131", app_build = "1793000000", available = false }
	for k, v in fields do
		now[k] = v
	end
	app:set_state("about", now)
	app:frames(6)
end

test("About says which Keiki this is, and that this app isn't what updates", function()
	local app = open(ABOUT)
	about(app, {})
	expect(shows(app, PAGE, "Not connected to the engine")).to_be(true)
	about(app, { engine_version = "0.2.131", engine_build = "abcdef1234.1793000000" })
	expect(shows(app, PAGE, '"0.2.131"')).to_be(true)
	expect(shows(app, PAGE, "abcdef1 · ")).to_be(true)
	expect(shows(app, PAGE, "Not packaged yet")).to_be(true)
	expect(shows(app, PAGE, "Updates go to the engine on this computer")).to_be(true)
	-- Copied for a bug report.
	app:click(`{PAGE} >> role:button "Copy"`)
	app:frames(6)
	-- (Found where it is: the whole tree's first 200 lines are the page's.)
	expect(app:has('"Version info copied"')).to_be(true)
	-- The links open in the browser, the notes at this version's.
	app:scroll(PAGE, 2000)
	app:frames(4)
	app:click(`{PAGE} >> role:button "Release notes"`)
	app:frames(4)
	expect(asked(app, "open_url:https://github.com/Mail-0/comet/releases/tag/v0.2.131")).to_be(true)
end)

test("an engine up to date says so and when it was checked; a check finds a newer one", function()
	local app = open(ABOUT)
	about(app, { engine_version = "0.2.131", latest = "0.2.131", checked_at = os.time() * 1000 })
	expect(shows(app, PAGE, "Keiki is up to date")).to_be(true)
	expect(shows(app, PAGE, "checked just now")).to_be(true)
	-- Checking: said while the feed is asked.
	app:set_state("settings_stub_about_next", "pending")
	app:click(`{PAGE} >> role:button "Check for updates"`)
	app:frames(6)
	expect(shows(app, PAGE, "Checking for updates…")).to_be(true)
	expect(app:has(`{PAGE} >> role:button "Check for updates"`)).to_be(false)
	-- Offline: said, and asked again.
	app:set_state("about_doing", "")
	app:set_state("settings_stub_about_next", "offline")
	app:click(`{PAGE} >> role:button "Check for updates"`)
	app:frames(6)
	expect(shows(app, PAGE, "This computer seems to be offline")).to_be(true)
	app:set_state("settings_stub_about_next", "ok")
	app:click(`{PAGE} >> role:button "Try again"`)
	app:frames(6)
	expect(asked(app, "about.check")).to_be(true)
	-- What the host then says: a newer release.
	about(app, { engine_version = "0.2.131", latest = "0.2.140", available = true, checked_at = os.time() * 1000 })
	expect(shows(app, PAGE, "Keiki 0.2.140 is available")).to_be(true)
	expect(shows(app, PAGE, "You have 0.2.131")).to_be(true)
end)

test("updating the engine says so, then that it runs the new version; the app's own engine sends you to the app", function()
	local app = open(ABOUT)
	about(app, { engine_version = "0.2.131", latest = "0.2.140", available = true })
	app:set_state("settings_stub_about_next", "pending")
	app:click(`{PAGE} >> role:button "Update engine"`)
	app:frames(6)
	expect(shows(app, PAGE, "Updating the engine…")).to_be(true)
	expect(app:has(`{PAGE} >> role:button "Update engine"`)).to_be(false)
	-- Done: starting again, then running it.
	app:set_state("about_doing", "")
	app:set_state("settings_stub_about_next", "ok")
	app:click(`{PAGE} >> role:button "Update engine"`)
	app:frames(6)
	expect(asked(app, "about.apply")).to_be(true)
	expect(shows(app, PAGE, "Updated to Keiki 0.2.140")).to_be(true)
	expect(shows(app, PAGE, "The engine is starting again on it")).to_be(true)
	about(app, { engine_version = "0.2.140", latest = "0.2.140", available = false })
	expect(shows(app, PAGE, "The engine is running the new version")).to_be(true)
	-- The Zeron app's own engine: updated from the app.
	app:set_state("about_doing", "")
	about(app, { engine_version = "0.2.131", latest = "0.2.140", available = true })
	app:set_state("settings_stub_about_next", "unmanaged")
	app:click(`{PAGE} >> role:button "Update engine"`)
	app:frames(6)
	expect(shows(app, PAGE, "Update from the Keiki app")).to_be(true)
	-- The engine's own check failing, with none here since.
	app:set_state("about_doing", "")
	about(app, { engine_version = "0.2.131", error = "feed unreachable" })
	expect(shows(app, PAGE, "The engine couldn't reach the release feed")).to_be(true)
end)




test("Reduce motion in Appearance is the one setting every move asks", function()
	local app = open("Appearance")
	expect(app:state("motion_reduce")).to_be(false)
	app:scroll(PAGE, 500)
	app:frames(4)
	app:click(`{PAGE} >> role:switch "Reduce motion"`)
	app:frames(6)
	expect(app:state("motion_reduce")).to_be(true)
	app:click(`{PAGE} >> role:switch "Reduce motion"`)
	app:frames(6)
	expect(app:state("motion_reduce")).to_be(false)
end)

test("a signed-out account offers Keiki sign-in without legacy account forms", function()
	local app = open("Account")
	app:command("settings_stub.signed_out")
	app:frames(20)
	expect(app:has(`{PAGE} >> role:button "Sign in"`)).to_be(true)
	for _, label in { "Name", "Title", "Username", "Delete account", "Sign out" } do
		expect(app:has(`{PAGE} >> role:button "{label}"`)).to_be(false)
		expect(app:has(`{PAGE} >> role:textbox "{label}"`)).to_be(false)
	end
	app:click(`{PAGE} >> role:button "Sign in"`)
	app:frames(5)
	expect(asked(app, "app.onboard")).to_be(true)
end)

-- Profile: your title, your photo and deleting your account.



-- Deleting: opened from its row, typed, confirmed. `word` is what the stub's
-- edge answers ("ok", or a refusal).
local DELETE_DIALOG = 'role:dialog "Delete your account?"'

local function deletes_asked(app: any): number
	local n = 0
	for _, c in calls(app) do
		n += if c == "account.delete" then 1 else 0
	end
	return n
end




-- Only just after signing in can the account be deleted (no agent on this
-- Mac can): the engine asks for it, the browser opens, and it goes once
-- you're back.



-- Signing keys: the keys your devices sign what you make with.
-- How many shown lines under `scope` hold `text` (a closing dialog's
-- hidden buttons aside).
local function count(app: any, scope: string, text: string): number
	local n = 0
	for line in app:tree(scope):gmatch("[^\n]+") do
		if line:find(text, 1, true) and not line:find(" hidden", 1, true) then
			n += 1
		end
	end
	return n
end







test("the pages are one tab list: the open one selected, the arrows between them; the search's hint reads as one", function()
	local app = open("Account")
	expect(app:has(`{NAV} >> role:tablist "Settings pages"`)).to_be(true)
	-- "Back to chats" is a way out, not a page.
	expect(app:has(`{NAV} >> role:button "Back to chats"`)).to_be(true)
	-- From the search, Tab lands on the open page (the selected tab), not
	-- the first; one more Tab leaves the list for the page.
	app:click(`{NAV} >> role:textbox "Search settings"`)
	app:key("Tab")
	app:frames(4)
	expect(app:tree(`{NAV} >> role:tab "Account"`)).to_contain("focused")
	expect(app:tree(`{NAV} >> role:tab "General"`)).never.to_contain("focused")
	-- ↓: the next page has the focus, still in the list.
	app:key("ArrowDown")
	app:frames(4)
	expect(app:tree(`{NAV} >> role:tab "Account"`)).never.to_contain("focused")
	expect(app:tree(`{NAV} >> role:tablist "Settings pages"`)).to_contain("focused")
	app:key("Tab")
	app:frames(4)
	-- (Out of the pages' list: on to the foot's you and settings.)
	expect(app:tree(`{NAV} >> role:tablist "Settings pages"`)).never.to_contain("focused")
	-- ⌘F, said once (Ctrl+F off a Mac, said as such).
	expect(app:has(`{NAV} >> #settings-search >> "Command F"`) or app:has(`{NAV} >> #settings-search >> "Ctrl F"`)).to_be(true)
end)

test("Shortcuts shows the Appshot hotkey while Appshots are on, as set, and sends you to change it", function()
	local app = open("Shortcuts")
	expect(shows(app, PAGE, "Capture Appshot")).to_be(false)
	app:set_state("appshots_enabled", true)
	app:frames(10)
	expect(shows(app, PAGE, "Capture Appshot")).to_be(true)
	-- (As this platform says the keys: macOS's names, or Ctrl, Alt elsewhere.)
	expect(app:has(`{PAGE} >> "Control Option Space"`) or app:has(`{PAGE} >> "Ctrl Alt Space"`)).to_be(true)
	app:set_state("appshots_shortcut", "cmd+shift+2")
	app:frames(10)
	-- (Said in the order macOS writes them: ⇧⌘2; elsewhere Ctrl+Shift+2.)
	expect(app:has(`{PAGE} >> "Shift Command 2"`) or app:has(`{PAGE} >> "Ctrl Shift 2"`)).to_be(true)
	-- (The wheel over the page's heading: its middle is past the window's foot.)
	app:scroll(`{PAGE} >> role:heading "Shortcuts"`, 3000)
	app:frames(4)
	app:click(`{PAGE} >> role:button "Change"`)
	app:frames(20)
	expect(app:has(`{NAV} >> role:tab "Appshots"`)).to_be(true)
	expect(shows(app, PAGE, "Capture Appshots")).to_be(true)
end)

test("Providers stands on the page's middle, as General does", function()
	local app = open("General")
	app:resize(1400, 900)
	app:frames(30)
	local gx = box(app, `{PAGE} >> role:heading "General"`)
	app:click(`{NAV} >> role:tab "Providers"`)
	app:frames(40)
	local px = box(app, `{PAGE} >> role:heading "Providers"`)
	expect(gx > 0).to_be(true)
	expect(math.abs(px - gx) < 1).to_be(true)
end)

-- A node's box under `scope`, by a line's text: { x, y, w, h }.
local function box_of(app: any, scope: string, part: string): { x: number, y: number, w: number, h: number }?
	for l in app:tree(scope):gmatch("[^\n]+") do
		if l:find(part, 1, true) then
			local x, y, w, h = l:match("(%-?[%d%.]+),(%-?[%d%.]+) ([%d%.]+)x([%d%.]+)")
			if x then
				return { x = tonumber(x) :: number, y = tonumber(y) :: number, w = tonumber(w) :: number, h = tonumber(h) :: number }
			end
		end
	end
	return nil
end

-- Each of Appearance's wallpaper states: one card, a hairline only
-- between rows that are there (none under the last), and every control
-- inline at its row's end (to the right of the words, on their line).
local NEW_CHAT = "#appearance-new-chat"
local function inline(app: any, label: string, control: string)
	local words, ctl = box_of(app, NEW_CHAT, `text "{label}"`), box_of(app, NEW_CHAT, control)
	expect(words ~= nil and ctl ~= nil).to_be(true)
	local w, c = words :: any, ctl :: any
	expect(c.x > w.x + 300).to_be(true)
	expect(math.abs((c.y + c.h / 2) - (w.y + w.h / 2)) < 24).to_be(true)
end

test("the wallpaper's rows stand inline in one card, a hairline only between them, in each state", function()
	local app = open("Appearance")
	app:resize(1200, 1800)
	-- None: one row, no hairline at all.
	app:set_state("shell_wallpaper", "none")
	app:set_state("shell_wallpaper_image", nil)
	app:frames(10)
	expect(app:count(`{NEW_CHAT} >> .settings-rule`)).to_be(0)
	inline(app, "Wallpaper", `button "Wallpaper"`)
	-- Image, none chosen yet: its row under a hairline; no effect to choose.
	app:set_state("shell_wallpaper", "image")
	app:frames(10)
	expect(app:count(`{NEW_CHAT} >> .settings-rule`)).to_be(2)
	inline(app, "Wallpaper", `button "Wallpaper"`)
	inline(app, "No image chosen", `button "Choose Image"`)
	-- A picture, plain and under an effect: three rows, two hairlines.
	for _, effect in { "none", "dither" } do
		app:set_state("shell_wallpaper_image", { path = "assets/favicons/zeron.png", name = "zeron.png", w = 512, h = 512, x = 0.5, y = 0.5, zoom = 1 })
		app:set_state("shell_wallpaper_effect", effect)
		app:frames(10)
		expect(app:count(`{NEW_CHAT} >> .settings-rule`)).to_be(2)
		inline(app, "Wallpaper", `button "Wallpaper"`)
		inline(app, "zeron.png", `button "Replace"`)
		inline(app, "Effect", `button "Effect"`)
	end
	expect(shows(app, NEW_CHAT, "Rebuilds the artwork with a dithered")).to_be(true)
	-- Back to None: the picture's rows go, hairlines with them.
	app:set_state("shell_wallpaper", "none")
	app:frames(10)
	expect(app:count(`{NEW_CHAT} >> .settings-rule`)).to_be(0)
	expect(app:state("shell_wallpaper_image")).never.to_be_nil()
end)


test("the Keiki account shows its identity and switches organizations without moving data", function()
	local app = open("Account")
	expect(shows(app, PAGE, "ada@example.net")).to_be(true)
	expect(shows(app, PAGE, "Ada Lovelace")).to_be(true)
	expect(app:has(`{PAGE} >> role:textbox "Name"`)).to_be(false)
	app:click(`{PAGE} >> #setting-organization >> role:button`)
	app:frames(5)
	app:click('role:menuitem "Compiler Museum"')
	app:frames(10)
	expect(asked(app, "account.choose_org:org_2")).to_be(true)
	expect(app:state("shell_route_path")).to_be("/settings/profile")
	expect(shows(app, PAGE, "Changing organizations does not move your local data.")).to_be(true)
end)

test("empty Keiki organizations disable the picker", function()
	local app = open("Account")
	app:set_state("settings_stub_org_mode", "empty")
	app:command("settings_stub.reload_orgs")
	app:frames(10)
	expect(app:state("settings_orgs")).to_equal({})
	expect(app:tree("#setting-organization")).to_contain("disabled")
end)

test("a rejected Keiki organization refresh leaves no stale picker choices", function()
	local app = open("Account")
	app:set_state("settings_stub_org_mode", "error")
	app:command("settings_stub.reload_orgs")
	app:frames(10)
	expect(app:state("settings_orgs_failure")).to_be("offline")
	expect(app:state("settings_orgs")).to_equal({})
	expect(app:tree("#setting-organization")).to_contain("disabled")
end)
