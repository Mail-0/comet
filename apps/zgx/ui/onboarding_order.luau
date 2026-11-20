-- Setup navigation with only Keiki sign-in, local appearance, and org selection.
local state = require("@screens/onboarding/state")
local actions = require("@screens/onboarding/actions")
local failures: { string } = {}
local current = ""

local function same(a: any, b: any): boolean
	if type(a) ~= "table" or type(b) ~= "table" then
		return a == b
	end
	if #a ~= #b then
		return false
	end
	for i = 1, #a do
		if a[i] ~= b[i] then
			return false
		end
	end
	return true
end

local function show(v: any): string
	if type(v) == "table" then
		return "{" .. table.concat(v, ", ") .. "}"
	end
	return tostring(v)
end

local function expect(actual: any)
	local function check(wanted: any)
		if not same(actual, wanted) then
			table.insert(failures, `{current}: got {show(actual)}, wanted {show(wanted)}`)
		end
	end
	return { to_be = check, to_equal = check }
end

local function test(name: string, body: () -> ())
	current = name
	local ok, err = pcall(body)
	if not ok then
		table.insert(failures, `{name}: {err}`)
	end
end

local function start(org_done: boolean, appearance_done: boolean): string
	state.path:set("signed_in")
	state.org_done:set(org_done)
	state.device_done:set(if appearance_done then { appearance = true } else {})
	state.step:set("signin")
	actions.start_run({})
	return state.step:peek()
end

test("a first Keiki sign-in visits appearance and organization selection", function()
	expect(start(false, false)).to_be("appearance")
	expect(state.order()).to_equal({ "signin", "appearance", "org", "done" })
	actions.next()
	expect(state.step:peek()).to_be("org")
	actions.next()
	expect(state.step:peek()).to_be("done")
	actions.back()
	expect(state.step:peek()).to_be("org")
	actions.back()
	expect(state.step:peek()).to_be("appearance")
	actions.back()
	expect(state.step:peek()).to_be("appearance")
end)

test("an existing active organization skips only its picker", function()
	expect(start(true, false)).to_be("appearance")
	expect(state.order()).to_equal({ "signin", "appearance", "done" })
	actions.next()
	expect(state.step:peek()).to_be("done")
end)

test("local appearance completion survives a later sign-in", function()
	expect(start(false, true)).to_be("org")
	expect(start(true, true)).to_be("done")
end)

test("sign-out always returns to the Keiki sign-in gate", function()
	start(true, true)
	actions.signed_out()
	expect(state.step:peek()).to_be("signin")
	expect(state.order()).to_equal({ "signin" })
	expect(state.org_done:peek()).to_be(false)
end)

return ui.text { id = "result", if #failures == 0 then "ok" else table.concat(failures, "\n") }
