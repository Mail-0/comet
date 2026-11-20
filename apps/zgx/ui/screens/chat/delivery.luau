-- Local durable-send rows and their queue labels.
local M = { LONG_WAIT = 3600 }
local function say(key: string, english: string, args: { [string]: any }?): string
	if (ui :: any) == nil then return english end
	return ui.t(`chat.cloud_machine.{key}`, args):get()
end

M.OUTBOX = "outbox:"

function M.outbox_rows(list: { any }?, removable: boolean): { any }
	local out = {}
	for _, w in list or {} do
		if w.held and w.id then
			table.insert(out, {
				id = M.OUTBOX .. w.id,
				text = w.text or "",
				waiting = if w.failure then say("outbox_failed", "Didn't send") else say("outbox_uploading", "Uploading…"),
				removable = removable,
			})
		end
	end
	return out
end

-- The outbox send a queue row stands for, if it is one.
function M.outbox_id(row: string): string?
	if string.sub(row, 1, #M.OUTBOX) == M.OUTBOX then
		return string.sub(row, #M.OUTBOX + 1)
	end
	return nil
end

-- Whether the list has any outbox send (worth asking the engine whether it
-- can take one back).
function M.has_outbox(list: { any }?): boolean
	for _, w in list or {} do
		if w.held then
			return true
		end
	end
	return false
end

-- What waits on its way to a chat (the engine's waiting messages, as the queue
-- takes them): those that still will run (not past `expires`), at when sent,
-- and the first that stopped trying, if one did.
function M.waiting_of(list: { any }?, now: number): ({ { at: number } }, { failure: string, message: string? }?)
	local out, failed = {}, nil
	for _, w in list or {} do
		if w.failure then
			failed = failed or { failure = w.failure, message = w.message }
		elseif w.sent and not (w.expires and w.expires <= now) then
			table.insert(out, { at = w.sent })
		end
	end
	return out, failed
end

-- "40 minutes ago", "5 hours ago", "3 days ago".
function M.age_words(secs: number): string
	if secs < 3600 then
		local n = math.max(1, secs // 60)
		return say("ago_minutes", if n == 1 then "1 minute ago" else `{n} minutes ago`, { n = n })
	elseif secs < 86400 then
		local n = secs // 3600
		return say("ago_hours", if n == 1 then "1 hour ago" else `{n} hours ago`, { n = n })
	end
	local n = secs // 86400
	return say("ago_days", if n == 1 then "1 day ago" else `{n} days ago`, { n = n })
end


return M
