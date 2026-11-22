-- Keiki sign-in, local appearance, and organization selection.
export type Step = "signin" | "appearance" | "org" | "done"
export type Path = "signed_in" | "skipped"
export type Appearance = "system" | "dark" | "light"
export type Account = "create" | "existing"
export type SignIn = "ready" | "waiting" | "failed"

local M = {}
M.PATHS = {
	signed_in = { "signin", "appearance", "org", "done" },
	skipped = { "signin" },
} :: { [Path]: { Step } }
M.PAGES = { appearance = "device" }
M.device_done = ui.persist("onboarding_done", {} :: { [string]: boolean })
M.passed = ui.state("passed", {} :: { [string]: boolean })
M.replay = ui.shared("onboarding.replay", false):peek() == true
local start = ui.shared("onboarding.start", "signin"):peek() :: Step
M.step = ui.state("step", start)
M.path = ui.state("path", "skipped" :: Path)
M.wall_again = ui.state("wall_again", false)
M.org_done = ui.state("org_done", false)
M.org_name = ui.state("org_name", "")
M.account = ui.state("account", "existing" :: Account)
M.signin = ui.state("signin", "ready" :: SignIn)
M.appearance = ui.persist("appearance", "system" :: Appearance)
M.last_org = ui.persist("last_org", "")

local visits = require("@app/screens/visits").new("onboarding", function(): string?
	return M.step:get()
end)
M.visit = visits.visit
M.once_a_visit = visits.once
M.ask_until_answered = visits.ask_until_answered
M.answered = visits.answered
M.answer = visits.answer

function M.order(): { Step }
	local out: { Step } = {}
	for _, step in M.PATHS[M.path:get()] do
		if step ~= "org" or not M.org_done:get() then
			table.insert(out, step)
		end
	end
	return out
end

function M.position(step: Step): (number?, number)
	local n: number? = nil
	local count = 0
	for _, current in M.order() do
		if current ~= "signin" then
			count += 1
			if current == step then n = count end
		end
	end
	return n, count
end

return M
