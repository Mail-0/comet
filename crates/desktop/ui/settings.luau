--!strict
local state = ui.shared("keiki.settings", { route = "chat" })
local settings = { state = state }
function settings.overlay(): { any }
	return {}
end
return settings
