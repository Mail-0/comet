-- Keiki Copilot uses the bundled Keiki mark.

export type Name = "copilot"
local FILES: { [Name]: string } = { copilot = "copilot-mark.png" }

-- Marks drawn in one ink (black, for the white tile): bare, they take the
-- colour of the text around them instead, or they vanish on dark.
local MONO: { [Name]: boolean } = {}

-- `bare`: the glyph alone, no tile (inline, beside text: a model's chip),
-- and then `name` may be a function the glyph follows. (`name` may be a
-- registry agent's harness, `acp.<id>`, too.)
export type Props = { name: Name | string | () -> (Name | string), size: number?, bare: boolean? }

-- The file of a mark (an SVG unless it names its own kind).
local function file(name: Name): string
	local f = FILES[name] or FILES.copilot
	return if string.find(f, "%.%a+$") then `assets/icons/{f}` else `assets/icons/{f}.svg`
end

-- A mark's picture at `side`: a raster drawn as it is, an SVG tinted when
-- it's one ink and `tint` is given.
local function picture(name: Name, side: number, tint: string?): Node
	local src = file(name)
	if string.sub(src, -4) == ".png" then
		return ui.image { src = src, w = side, h = side, shrink = 0, fit = "contain" }
	end
	return ui.svg { src = src, tint = if MONO[name] then tint else nil, w = side, h = side, shrink = 0 }
end

return function(props: Props): Node
	local size = props.size or 40
	local name = props.name
	if props.bare then
		if type(name) ~= "function" then
			return picture(name :: Name, size, "currentColor")
		end
		-- Following a name: a tinted glyph and a plain one, whichever the
		-- name's mark wants showing (a tint, once set, has no way back to
		-- the mark's own colours).
		return ui.presence(name :: any, function(now: string): Node
			return picture(now :: Name, size, "currentColor")
		end)
	end
	assert(type(name) == "string", "a tiled mark takes a name, not a function")
	return ui.column {
		w = size,
		h = size,
		-- The tile's corner is a quarter of its size, whatever the size.
		rounded = ui.raw(size * 0.25, "a mark tile's radius scales with it"),
		items = "center",
		justify = "center",
		bg = "$zeron.mark.bg",
		border = 1,
		border_color = "$zeron.mark.border",
		shadow = "$shadow.sm",
		picture(name :: Name, size * 0.56, nil),
	}
end
