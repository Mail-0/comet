--!strict
local kit = require("./kit")
local draft = ui.shared("keiki.composer", "")
local account = ui.shared("keiki.account", { busy = false })
local conversations = ui.shared("keiki.conversations", {} :: { agent_id: string? })
local composer = { state = draft }
function composer.main()
	return {
		kit.text_input({
			id = "message",
			label = "Message",
			placeholder = "Message a Keiki agent",
			value = draft,
		}),
		kit.button("Send", function()
			ui.invoke("conversations.send", draft:get())
			ui.invoke("composer.clear")
		end, function()
			return account:get().busy or conversations:get().agent_id == nil or draft:get() == ""
		end, "send"),
	}
end
return composer
