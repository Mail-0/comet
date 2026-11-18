--!strict
local state = ui.shared("keiki.updater", { available = false })
local updater = { state = state }
function updater.modals(): { any }
	return {}
end
return updater
