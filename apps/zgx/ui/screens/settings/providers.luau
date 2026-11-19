-- Settings → Providers: the device's Copilot descriptor.

local ds = require("@ds")
local agents = require("@account/agents")
local search = require("@app/screens/settings/search")
local devices_page = require("@app/screens/settings/devices")
local sessions = require("@account/sessions")
local visits = require("@app/screens/settings/visits")
local cloud = require("@account/cloud")

local M = {}

M.ID = "providers"
M.TITLE = "Providers"

search.index(M.ID, ui.t("settings.providers.title"), ui.t("settings.providers.terms"), {
    { key = "agents", title = ui.t("settings.providers.agents.label"), terms = ui.t("settings.providers.agents.terms") },
})

local function now(): agents.Now
	return agents.now:get() :: agents.Now
end

-- The agents shown: those with a mark (every one Zeron runs but a test rig),
-- and those added from the ACP registry.
local function shown(): { agents.Agent }
	local out = {}
	for _, a in now().list do
		if a.id == "copilot" then
			table.insert(out, a)
		end
	end
	return out
end

local function installed(): { agents.Agent }
	local out = {}
	for _, a in shown() do
		if a.installed then
			table.insert(out, a)
		end
	end
	return out
end

local function missing(): { agents.Agent }
	local out = {}
	for _, a in shown() do
		if not a.installed then
			table.insert(out, a)
		end
	end
	return out
end

-- Whose agents show: one of the account's computers (agents run on a Mac
-- or Linux, not a phone), this one first. Choosing one asks the engine to
-- address every agent call there (`agents.target`).
local function computers(): { sessions.Device }
	return sessions.computers(sessions.devices:get(), true)
end
local function here(): string?
	for _, d in sessions.now:peek().devices or {} do
		if d.here then
			return d.id
		end
	end
	return nil
end
-- Cloud, as one choice after the computers (never its machines): the
-- accounts every Cloud machine uses, kept by the vault, which the engine
-- answers for (crates/engine cloud_accounts: Claude Code and Codex, their
-- logins and API keys). Offered while Cloud is on.
local CLOUD = "cloud"
local function on_cloud(): boolean
	return now().target == CLOUD
end
local function cloud_choice(): { any }
	if cloud.on() or on_cloud() then
		return { { value = CLOUD, label = ui.t("settings.providers.cloud"):get(), icon = "cloud" } }
	end
	return {}
end
-- The device shown: the engine's target, else this one.
local function shown_device(): string?
	return now().target or here()
end
-- (In the locale shown: read where it's drawn.)
local function device_name(): string
	local id = shown_device()
	if id == CLOUD then
		return ui.t("settings.providers.cloud"):get()
	end
	for _, d in computers() do
		if d.id == id then
			return if d.here then ui.t("settings.providers.this_mac"):get() else d.name
		end
	end
	return ui.t("settings.providers.this_mac"):get()
end
local function choose_device(id: string)
	agents.target(if id == here() then nil else id)
end

local function kind(): string
	local s = now().state
	if s == "looking" or s == "unavailable" then
		return s
	end
	-- (Cloud's page is another: its accounts, its keys, nothing to add.)
	return if now().target == "cloud" then "cloud" else "found"
end

local function card(row: State<agents.Agent>): Node
	local first = row:peek()
	local id = first.id
	local cloud_card = on_cloud()
	-- The switch follows the engine; a flip goes to it, and its answer
	-- comes back in the row.
	local on = ui.use_state(first.enabled)
	local engine_said = first.enabled
	ui.effect(function()
		local said, flipped = row:get().enabled, on:get()
		if said ~= engine_said then
			engine_said = said
			if flipped ~= said then
				on:set(said)
			end
		elseif flipped ~= said and not cloud_card then
			agents.set_enabled(id, flipped)
		end
	end)
    return ds.ProviderCard.Card {
        provider = { id = id, name = first.name, mark = first.mark },
        enabled = on,
        settings = false,
        sign_in_only = true,
        login_methods = function() return {} end,
        accounts = function() return {} end,
        foot = function() return "idle" end,
        on_add = function() agents.sign_in(id) end,
    }
end

local function cards(): Node
	return search.anchor(
		"agents",
		ui.column {
			w_full = true,
			gap = "$space.4",
			ui.each(ui.computed(installed), function(_a: agents.Agent, _i: number, row: State<agents.Agent>): Node
				return card(row)
			end, {
				-- (By the device shown too: a card is made for its device,
				-- Cloud's unlike a computer's.)
				key = function(a: agents.Agent): string
					return `{now().target or ""}|{a.id}`
				end,
			}),
		}
	)
end

local function body(k: string): Node
	if k == "looking" then
		return ds.ProviderCard.Loading()
	elseif k == "unavailable" then
		return ds.ProviderCard.Unavailable(function()
			return device_name()
		end, agents.load)
	end
	if k == "cloud" then
		return ui.column { w_full = true, gap = "$space.6", cards() }
	end
	return ui.column { w_full = true, gap = "$space.6", cards() }
end

-- The agents' page: the cards, then the agents still to add.
local function agents_page(): Node
	return ds.Settings.Page {
		title = ui.t("settings.providers.title"),
		description = ui.t("settings.providers.description"),
		-- Agents are per device: which one's (a choice once there's more
		-- than this one).
		-- (The same choice as Plugins': screens/settings/devices.)
		trail = devices_page.picker(shown_device, choose_device, cloud_choice),
		-- (At the top right, on the title's line, as Plugins' is.)
		trail_top = true,
		ui.presence(kind, body, { w_full = true, enter = { opacity = { 0, 1 }, duration = 120 }, exit = { opacity = 0, duration = 60 }, mode = "wait" }),
	}
end

function M.view(): Node
    visits.once("providers", agents.load)
    return agents_page()
end

return M
