local ds = require("@ds")
local keiki = require("@account/keiki")
local auth = require("@account/auth")

return function(agent: string, name: string, size: number?): Node
	local image = ui.resource(function(): any
		local revision = keiki.revision:get()
		local account = auth.account:get()
		local _ = ui.theme.current:get()
		local theme = if string.find(ui.theme.kind(), "light", 1, true) then "light" else "dark"
		local detail = keiki.conversation:get()
		local state = if detail
				and detail.agent
				and detail.agent.id == agent
				and detail.liveness
			then "running"
			else "idle"
		if account.state ~= "signed_in" or revision == 0 then
			return nil
		end
		return keiki.call("GetKeikiAvatar", { agentId = agent, state = state, theme = theme }):await()
	end)
	return ds.Avatar({
		seed = agent,
		label = name,
		size = size or 20,
		src = function()
			local got = image.data:get()
			return if got then got.src else nil
		end,
	})
end
