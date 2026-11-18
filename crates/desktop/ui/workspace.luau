--!strict
type Workspace = { status: string?, names: { string }? }
return ui.shared("keiki.workspace", {} :: Workspace)
