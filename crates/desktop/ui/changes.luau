--!strict
local state = ui.shared("keiki.changes", { available = false })
local changes = { state = state }
function changes.panels(): { any }
	return {}
end
return changes
