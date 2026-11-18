--!strict
local state = ui.shared("keiki.copilot", { available = false })
local copilot = { state = state }
function copilot.sidebar(): { any }
	return {}
end
return copilot
