--!strict
type Conversations = { agent_id: string?, busy: boolean? }
return ui.shared("keiki.conversations", {} :: Conversations)
