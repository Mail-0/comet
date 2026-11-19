local ds = require("@ds")
local shared = require("@app/screens/chat/shared")
local checked = ui.state("fixture_dock_checks", 0)
local stale = ui.state("fixture_stale_dock", 0)
local Composer = ds.Composer

ds.Composer = function(props: any): Node
	local on_sizes = props.on_sizes
	if on_sizes then
		props.on_sizes = function(sizes: any)
			on_sizes(sizes)
			checked:add(1)
			if math.abs(shared.dock_h:peek() - sizes.presented) > 0.01 then stale:add(1) end
		end
	end
	return Composer(props)
end

if ui.shared("fixture_dock_v2", false):peek() then
	ds.setup()
	local chat = require("@app/screens/chat")
	chat.Composer = require("@app/templates/zeron-v2/screens/chat/composer")
end

return require("./chat_fixture")
