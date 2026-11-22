-- Copilot uses the bundled mark; historical agents use their initial.

local Mark = require("@self/mark")
local Tile = require("@self/tile")

export type Props = {
	label: string,
	mark: string?,
	icon: string?,
	src: string?,
	seed: string?,
	size: number?,
	bare: boolean?,
}

local M = {}

function M.look(props: Props): any
    return if props.mark == "copilot" or props.seed == "copilot"
        then { kind = "builtin", mark = "copilot" }
        else { kind = "initial" }
end

function M.Mark(props: Props): Node
    local size = props.size or 32
    if M.look(props).kind == "builtin" then
        return Mark { name = "copilot", size = size, bare = props.bare }
    end
    return Tile { label = props.label, size = size }
end

-- Called as the component itself (`AgentMark { … }`).
return setmetatable(M, {
	__call = function(_self: any, props: Props): Node
		return M.Mark(props)
	end,
}) :: any
