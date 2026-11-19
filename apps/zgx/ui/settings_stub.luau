-- Local settings and Keiki account responses for the mounted shell tests.
local host = require("@app/screens/settings/host")
local api = require("@account/api")
local auth = require("@account/auth")
local M = {}
local calls = ui.state("settings_stub_calls", {} :: { string })
local refuse = ui.state("settings_stub_refuse", false)
local about_next = ui.state("settings_stub_about_next", "ok")
local org_mode = ui.state("settings_stub_org_mode", "some")
local function log(entry: string) calls:push(entry) end
local function settled(value: any, word: string?): any
	local p = { done = true, value = if word then nil else value, error = if word then { message = word } else nil }
	function p.after(self: any, ok: ((any) -> ())?, bad: ((any) -> ())?)
		if word then
			if bad then
				bad({ message = word })
			end
		elseif ok then
			ok(value)
		end
	end
	return p
end

-- A change: refused if asked to be, else `value`.
local function change(value: any): any
	if refuse:peek() then
		refuse:set(false)
		return settled(nil, "refused")
	end
	return settled(value)
end

local engine: { [string]: any } = {
	GetCommitCredit = { enabled = false, cloud = false },
	GetWorktreeSettings = {
		settings = { useCustomDirectory = false },
		defaultDirectory = "~/.zeron/worktrees",
		effectiveDirectory = "~/.zeron/worktrees",
	},
	GetTitleSettings = {},
	GetNativeSubagents = { switchable = { "copilot" }, off = {} },
	GetBackgroundSettings = { keepRunningAfterQuit = true },
}
local banners = { desktop = true, background_only = true, agent_updates = true }

local answers: { [string]: (arg: any) -> any } = {
	["dictation.call"] = function(arg: any)
		return settled(if arg.op == "status" then { installed = false, size = 670479942 } elseif arg.op == "inputs" then {} else nil)
	end,
	["settings.engine"] = function(arg: any)
		local m = arg.method
		if string.sub(m, 1, 3) == "Set" then
			local r = change(nil)
			if r.error then
				return r
			end
			local get = "Get" .. string.sub(m, 4)
			if m == "SetCommitCredit" then
				engine[get] = { enabled = arg.params.enabled, cloud = false }
			elseif m == "SetWorktreeSettings" then
				local w = table.clone(engine[get])
				w.settings = arg.params
				engine[get] = w
			elseif m == "SetTitleSettings" or m == "SetBackgroundSettings" then
				engine[get] = arg.params
			end
			return settled(engine[get])
		end
		return settled(engine[m])
	end,
	["notifications.get"] = function()
		if refuse:peek() then
			refuse:set(false)
			return settled(nil, "failed")
		end
		return settled(table.clone(banners))
	end,
	["notifications.set"] = function(arg: any)
		local r = change(nil)
		if not r.error then
			for k, v in arg do
				banners[k] = v
			end
		end
		return r
	end,
	["about.check"] = function()
		local next = about_next:peek()
		if next == "pending" then
			-- (Still on its way: never settles.)
			return { done = false, after = function() end }
		elseif next ~= "ok" then
			return settled(nil, next)
		end
		return settled({ latest = "0.2.140", available = true })
	end,
	["about.apply"] = function()
		local next = about_next:peek()
		if next == "pending" then
			-- (Still on its way: never settles.)
			return { done = false, after = function() end }
		elseif next ~= "ok" then
			return settled(nil, next)
		end
		return settled({ version = "0.2.140" })
	end,
	["fonts.list"] = function()
		return settled({
			{ family = "Fira Code", mono = true },
			{ family = "Inter", mono = false },
			{ family = "JetBrains Mono", mono = true },
			{ family = "Noto Serif", mono = false },
		})
	end,
	["wallpaper.keep"] = function(arg: any)
		local name = string.match(arg.path, "[^/]+$") or arg.path
		return settled({ path = arg.path, name = name, w = 512, h = 512 })
	end,
}

local function detail(arg: any): string
	if type(arg) == "string" then
		return arg
	elseif type(arg) == "table" then
		return arg.id or arg.method or arg.name or arg.path or ""
	end
	return ""
end

local stub = host :: any
stub.call = function(name: string, arg: any?): any
	log(if detail(arg) ~= "" then `{name}:{detail(arg)}` else name)
	local answer = answers[name]
	return if answer then answer(arg) else settled(nil)
end
stub.invoke = function(name: string, arg: any?)
	log(if detail(arg) ~= "" then `{name}:{detail(arg)}` else name)

end
-- A photo picked: the app's own PNG, so a headless host can draw it.
stub.open_files = function(_opts: any): any
	log("open_files")
	return settled({ { path = "assets/favicons/zeron.png", name = "zeron.png", size = 1, type = "image/png" } })
end


local apis = api :: any
apis.orgs = function(): any
	log("account.orgs")
	if org_mode:peek() == "empty" then return settled({}) end
	if org_mode:peek() == "error" then return settled(nil, "offline") end
	return settled({
		{ id = "org_1", organization_id = "org_1", name = "Analytical Engines" },
		{ id = "org_2", organization_id = "org_2", name = "Compiler Museum" },
	})
end
apis.choose_org = function(id: string): any
	log(`account.choose_org:{id}`)
	local result = change(nil)
	if not result.error then
		auth.account:set_key("organization_id", id)
	end
	return result
end
ui.command {
	id = "settings_stub.reload_orgs", title = "Reload organizations",
	run = function() require("@app/screens/settings/store").load_org() end,
}
local agents = require("@account/agents")
local agent_stub = agents :: any
local function agents_of(): any
	return { state = "ready", target = nil, list = {
		{ id = "copilot", name = "Copilot", mark = "copilot", installed = true, can_install = false,
			enabled = true, version = nil, installing = false, login = "idle", accounts = {} },
	} }
end
agent_stub.load = function() log("agents.load"); agents.now:set(agents_of()) end
agent_stub.target = function(_device: string?) log("agents.target:"); agents.now:set(agents_of()) end
-- Renaming a device (account/sessions), as the engine would answer.
local sessions_stub = require("@account/sessions") :: any
sessions_stub.rename_device = function(id: string, name: string): any
	log(`sessions.rename_device:{id}:{name}`)
	return change(nil)
end

local auths = auth :: any
auths.sign_out = function(done: (() -> ())?)
	log("account.sign_out")
	auth.account:set({ state = "signed_out", env = "keiki" } :: any)
	if done then
		done()
	end
end

-- The launch before the engine has answered, and the engine saying signed out.
ui.command {
	id = "settings_stub.restoring",
	title = "The engine hasn't answered yet",
	run = function()
		auth.account:set({ state = "restoring", env = "keiki" } :: any)
	end,
}
ui.command {
	id = "settings_stub.signed_out",
	title = "The engine says signed out",
	run = function()
		auth.account:set({ state = "signed_out", env = "keiki" } :: any)
	end,
}

ui.command {
	id = "settings_stub.sign_in",
	title = "Signed in",
	run = function()
		auth.account:set({
			state = "signed_in",
			env = "keiki",
			user = { id = "ada@example.net", email = "ada@example.net", first_name = "Ada", name = "Ada Lovelace" },
			organization_id = "org_1",
		} :: any)
	end,
}
ui.command {
	id = "settings_stub.open_account",
	title = "Open Settings' old Account page",
	run = function()
		require("@app/screens/shell/route").settings("account")
	end,
}
-- What the main window's theme overrides now (ds.look): its accent, a
-- syntax colour and its fonts, for a test to read.
local window_theme = ui.state("settings_stub_window_theme", {} :: { [string]: any })
ui.command {
	id = "settings_stub.read_theme",
	title = "Read the window's theme",
	run = function()
		local t = require("@ds/look").window_theme() or {}
		window_theme:set({
			accent = t.accent,
			keyword = t["zeron.syntax.keyword"],
			body = t["font.size.md"],
			window = t["zeron.window"],
			sans = t["font.sans"],
			mono = t["font.mono"],
			secondary = t["text.secondary"],
			tertiary = t["text.tertiary"],
			-- (The page's and sidebar's fill and edge, when the window's
			-- theme says its own.)
			page_bg = t["zeron.inset.bg"],
			page_edge = t["zeron.inset.edge"],
			-- "Accent colour throughout": a status dot, a find's highlight,
			-- an @file token, a list marker, and a link's ink (nil: the
			-- text's own).
			working_dot = t["zeron.status.dot.working"],
			working = t["zeron.status.working"],
			find = t["zeron.editor.match"],
			file_token = t["zeron.composer.token.file"],
			marker = t["zeron.prose.marker"],
			link = require("@ds/look").accented_ink(nil)(),
			-- What the accent's own token is, and what the theme's says.
			accented_ink = ui.theme.get("zeron.accented.ink"),
			accent_ink = ui.theme.get("accent"),
		})
	end,
}
ui.command {
	id = "settings_stub.refuse_next",
	title = "Refuse the next change",
	run = function()
		refuse:set(true)
	end,
}
return M
