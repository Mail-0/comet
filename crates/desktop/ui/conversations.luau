--!strict
local kit = require("./kit")
type Entry = { id: string, name: string? }
type Account = { busy: boolean, agents: { Entry } }
type Conversations = { agent_id: string?, busy: boolean? }
local state = ui.shared("keiki.conversations", {} :: Conversations)
local account = ui.shared("keiki.account", { busy = false, agents = {} } :: Account)
local agents = ui.computed(function()
	return account:get().agents
end)
local conversations = { state = state }
function conversations.sidebar()
	return {
		ui.text({ "Keiki agents", bold = true }),
		ui.each(agents, function(agent)
			return kit.list_row(agent.name or agent.id, function()
				ui.invoke("conversations.choose_agent", agent.id)
				ui.invoke("composer.clear")
			end, function()
				return account:get().busy or state:get().agent_id == agent.id
			end)
		end, {
			key = function(agent)
				return agent.id
			end,
			gap = 6,
		}),
	}
end
function conversations.tabs(): { any }
	return {}
end
return conversations
