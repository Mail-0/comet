--!strict
local state = ui.shared("keiki.transcript", { reply = "" })
local transcript = { state = state }
function transcript.main()
	return {
		ui.scroll({
			grow = 1,
			min_h = 0,
			ui.text({
				function()
					return state:get().reply
				end,
				id = "reply",
				wrap = true,
			}),
		}),
	}
end
return transcript
