--!strict
local state = ui.shared("keiki.sandbox", { available = false })
local sandbox = { state = state }
function sandbox.panels(): { any }
	return {}
end
return sandbox
