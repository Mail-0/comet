--!strict
local kit = require("./kit")
local account = require("./account")
local workspace = require("./workspace")
local conversations = require("./conversations")
local transcript = require("./transcript")
local composer = require("./composer")
local copilot = require("./copilot")
local sandbox = require("./sandbox")
local changes = require("./changes")
local settings = require("./settings")
local updater = require("./updater")

local left_sidebar = ui.scroll({
	w = 270,
	shrink = 0,
	bg = kit.colors.surface,
	ui.column(kit.children({ p = kit.spacing.sidebar, gap = kit.spacing.gap }, {
		workspace.sidebar(),
		account.sidebar(),
		conversations.sidebar(),
		copilot.sidebar(),
	})),
})
local main_area = ui.column(kit.children({
	grow = 1,
	min_w = 0,
	p = kit.spacing.main,
	gap = 16,
}, {
	account.header(),
	conversations.tabs(),
	account.status(),
	transcript.main(),
	composer.main(),
}))
local right_bottom_panels = kit.children({}, { sandbox.panels(), changes.panels() })
local settings_route_overlay = settings.overlay()
local modal_layer = kit.children({}, { account.modals(), updater.modals() })

return ui.row(kit.children({
	w_full = true,
	h_full = true,
	bg = kit.colors.page,
	color = kit.colors.text,
	left_sidebar,
	main_area,
}, { right_bottom_panels, settings_route_overlay, modal_layer }))
