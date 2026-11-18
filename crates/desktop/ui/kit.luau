--!strict
local kit = {}
kit.colors = {
	page = "#201b18",
	surface = "#2c2420",
	raised = "#40312a",
	accent = "#e9a47f",
	muted = "#c1aa9c",
	text = "#fff5ef",
	hover = "#553b30",
}
kit.spacing = { sidebar = 20, main = 24, gap = 12 }

function kit.button(label: string, action: () -> (), disabled: () -> boolean, id: string?)
	return ui.button({
		id = id,
		px = 12,
		py = 8,
		rounded = 7,
		bg = kit.colors.raised,
		hover = { bg = kit.colors.hover },
		disabled = disabled,
		on_click = action,
		ui.text({ label, literal = true, wrap = true }),
	})
end

kit.list_row = kit.button

function kit.text_input(options: any)
	options.bg = options.bg or kit.colors.surface
	options.px = options.px or 12
	options.py = options.py or 10
	return ui.input(options)
end

function kit.icon(glyph: string)
	return ui.text({ glyph, literal = true })
end

function kit.children(props: any, views: { { any } }): any
	for _, children in views do
		for _, child in children do
			table.insert(props, child)
		end
	end
	return props
end

return kit
