--!strict
local kit = require("./kit")
type Workspace = { status: string?, names: { string }? }
local state = ui.shared("keiki.workspace", {} :: Workspace)
local spaces = ui.computed(function()
	return state:get().names or {}
end)
local workspace = { state = state }
function workspace.sidebar()
	return {
		ui.text({ "Keiki", text_size = 25, bold = true, color = kit.colors.accent }),
		ui.text({ "Desktop", color = kit.colors.muted }),
		ui.text({ "Local workspace", bold = true }),
		ui.text({
			function()
				return state:get().status
			end,
			color = kit.colors.muted,
		}),
		ui.each(spaces, function(name)
			return ui.text({ name, literal = true, wrap = true })
		end),
	}
end
return workspace
