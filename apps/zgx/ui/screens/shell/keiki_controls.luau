local ds = require("@ds")
local keiki = require("@account/keiki")
local route = require("@app/screens/shell/route")
local Avatar = require("@app/screens/shell/keiki_avatar")
local M = {}
M.open = ui.state("keiki.controls.open", false)
M.context = ui.state("keiki.controls.context", {} :: any)
M.info = ui.state("keiki.controls.info", nil :: any)
M.busy = ui.state("keiki.controls.busy", false)
M.error = ui.state("keiki.controls.error", nil :: string?)
M.templates = ui.state("keiki.controls.templates", {} :: any)
M.loading = ui.state("keiki.controls.loading", false)
local serial = 0
local function say(text: string)
	ds.toast.show({ title = text, tone = "neutral" })
end
local function call(method: string, params: any, done: (any) -> ())
	serial += 1
	local mine, revision = serial, keiki.revision:peek()
	M.busy:set(true)
	M.error:set(nil)
	keiki.call(method, params):after(function(value)
		if mine ~= serial or revision ~= keiki.revision:peek() then
			return
		end
		M.busy:set(false)
		done(value)
	end, function(error)
		if mine ~= serial or revision ~= keiki.revision:peek() then
			return
		end
		M.busy:set(false)
		M.loading:set(false)
		M.error:set(error.message or "Could not complete this action. Retry.")
	end)
end
local function show(context: any)
	serial += 1
	M.context:set(context)
	M.info:set(nil)
	M.error:set(nil)
	M.busy:set(false)
	M.loading:set(false)
	M.open:set(true)
end
function M.conversation(agent: string?, identity: string?, chat: string?)
	show({ kind = "conversation", agent = agent, identity = identity, chat = chat })
	local function load(id: string)
		M.context:set_key("chat", id)
		call("KeikiConversationInfo", { chatId = id, includeRemote = true }, function(info)
			M.info:set(info)
		end)
	end
	if chat then
		load(chat)
	else
		call("OpenKeikiConversation", { agentId = agent, conversationId = identity }, function(answer)
			load(answer.chatId)
		end)
	end
end
function M.group(id: string, name: string)
	show({ kind = "roster", group = id, name = name })
	keiki.roster(id)
end
function M.agent(id: string, name: string)
	show({ kind = "agent", agent = id, name = name })
end
function M.create()
	show({ kind = "create" })
	M.loading:set(true)
	call("ListKeikiTemplates", {}, function(answer)
		M.templates:set(answer.templates or {})
		M.loading:set(false)
	end)
end
function M.tasks(chat: string)
	show({ kind = "tasks", chat = chat })
end
local function action(action: string, extra: any?)
	local context = M.context:peek()
	local params = extra or {}
	params.chatId, params.action = context.chat, action
	call("KeikiConversationAction", params, function(answer)
		if action == "deleteLocal" then
			if route.current:peek().session == context.chat then
				route.new_chat()
			end
			M.open:set(false)
			say("Local entry deleted. Remote history is unchanged.")
		elseif action == "openDashboard" then
			say("Opened in your browser")
		elseif action == "block" or action == "unblock" or action == "end" then
			M.open:set(false)
			keiki.retry_conversation()
			say(
				if action == "end" then `Ended thread · {answer.settled or 0} asks settled` else "Conversation updated"
			)
		else
			local previous = M.info:peek() or {}
			local updated = table.clone(answer)
			updated.blocked, updated.takeover = previous.blocked, previous.takeover
			M.info:set(updated)
			M.context:set_key("kind", "conversation")
		end
		local agent = context.agent or (M.info:peek() or {}).agentId
		if agent then
			keiki.page(agent)
		end
	end)
end
local function button(label: string, click: () -> (), tone: any?): Node
	return ds.Button({
		label = label,
		tone = tone or "secondary",
		size = "sm",
		disabled = M.busy,
		on_click = click,
		ui.text({ label }),
	})
end
local function confirm(kind: string, title: string, body: string)
	M.context:set_key("kind", kind)
	M.context:set_key("title", title)
	M.context:set_key("body", body)
end
local function conversation(): Node
	return ui.column({
		w_full = true,
		gap = "$space.2",
		ui.show(
			function()
				return M.info:get() == nil and not M.busy:get()
			end,
			button("Retry conversation actions", function()
				local c = M.context:peek()
				M.conversation(c.agent, c.identity, c.chat)
			end)
		),
		ui.show(
			function()
				return M.info:get() ~= nil
			end,
			ui.column({
				w_full = true,
				gap = "$space.2",
				ui.text({
					function()
						return (M.info:get() or {}).conversationId or ""
					end,
					wrap = true,
					color = "$text.secondary",
					text_size = "$font.size.sm",
				}),
				button("Copy conversation ID", function()
					ui.clipboard.write(M.info:peek().conversationId)
					say("Conversation ID copied")
				end),
				button("View conversation in browser", function()
					action("openDashboard")
				end),
				button("Rename local entry", function()
					M.context:set_key("kind", "rename")
				end),
				ds.Button({
					label = function()
						return if (M.info:get() or {}).pinned then "Unpin conversation" else "Pin conversation"
					end,
					tone = "secondary",
					size = "sm",
					disabled = M.busy,
					on_click = function()
						action(if M.info:peek().pinned then "unpin" else "pin")
					end,
					ui.text({
						function()
							return if (M.info:get() or {}).pinned then "Unpin conversation" else "Pin conversation"
						end,
					}),
				}),
				ds.Button({
					label = function()
						return if (M.info:get() or {}).archived then "Restore local entry" else "Archive local entry"
					end,
					tone = "secondary",
					size = "sm",
					disabled = M.busy,
					on_click = function()
						action(if M.info:peek().archived then "restore" else "archive")
					end,
					ui.text({
						function()
							return if (M.info:get() or {}).archived
								then "Restore local entry"
								else "Archive local entry"
						end,
					}),
				}),
				ui.show(
					function()
						local info = M.info:get()
						return info ~= nil and not info.peer
					end,
					ds.Button({
						label = function()
							return if (M.info:get() or {}).blocked then "Unblock conversation" else "Block conversation"
						end,
						tone = "secondary",
						size = "sm",
						disabled = M.busy,
						on_click = function()
							action(if (M.info:peek() or {}).blocked then "unblock" else "block")
						end,
						ui.text({
							function()
								return if (M.info:get() or {}).blocked
									then "Unblock conversation"
									else "Block conversation"
							end,
						}),
					})
				),
				ui.show(
					function()
						local info = M.info:get()
						return info ~= nil and not info.peer and not info.desktop
					end,
					ds.Button({
						label = function()
							return if (M.info:get() or {}).takeover
								then "Hand back to agent"
								else "Take over conversation"
						end,
						tone = "secondary",
						size = "sm",
						disabled = M.busy,
						on_click = function()
							call("SetKeikiTakeover", {
								chatId = M.context:peek().chat,
								enabled = not (M.info:peek() or {}).takeover,
							}, function()
								M.open:set(false)
								keiki.retry_conversation()
							end)
						end,
						ui.text({
							function()
								return if (M.info:get() or {}).takeover
									then "Hand back to agent"
									else "Take over conversation"
							end,
						}),
					})
				),
				ui.show(
					function()
						return (M.info:get() or {}).peer == true
					end,
					button("End peer thread", function()
						confirm(
							"end",
							"End peer thread?",
							"Settle waiting asks and end this thread. Its history is retained."
						)
					end)
				),
				button("Delete local entry", function()
					confirm(
						"deleteLocal",
						"Delete local entry?",
						"This removes the local entry only. It does not delete or clear the Keiki conversation."
					)
				end),
			})
		),
	})
end
local function roster(): Node
	local function current(): any
		return keiki.rosters:get()[M.context:get().group] or {}
	end
	local members = ui.computed(function()
		return current().members or {}
	end)
	return ui.column({
		w_full = true,
		gap = "$space.3",
		ui.text({
			function()
				local r = current()
				return if r.loading
					then "Loading roster…"
					elseif r.error then r.error
					else string.format(
						"%d members · %d running · %d waiting · %d tasks · $%.2f",
						(r.totals or {}).members or #(r.members or {}),
						(r.totals or {}).running or 0,
						(r.totals or {}).waiting or 0,
						(r.totals or {}).backgroundTasks or 0,
						(r.totals or {}).costUsd or 0
					)
			end,
			wrap = true,
			color = "$text.secondary",
		}),
		ui.scroll({
			w_full = true,
			max_h = 300,
			ui.each(members, function(member: any, _i: number, now: State<any>): Node
				return ds.Surface({
					kind = "sunken",
					p = "$space.3",
					ui.row({
						gap = "$space.3",
						items = "center",
						Avatar(member.agentId, member.name, 28),
						ui.column({
							grow = 1,
							gap = "$space.1",
							ui.text({
								function()
									return now:get().name
								end,
								weight = "$font.weight.medium",
							}),
							ui.text({
								function()
									local m = now:get()
									return `{m.role or "member"} · {m.status or "idle"} · {m.runningTurns or 0} turns · {m.pendingAsks or 0} asks · {m.backgroundTasks or 0} tasks`
								end,
								wrap = true,
								text_size = "$font.size.sm",
								color = "$text.secondary",
							}),
							ui.text({
								function()
									local m = now:get()
									return string.format(
										"%d in / %d out tokens · $%.2f",
										m.tokensIn or 0,
										m.tokensOut or 0,
										m.costUsd or 0
									)
								end,
								text_size = "$font.size.sm",
								color = "$text.secondary",
							}),
						}),
					}),
				})
			end, {
				key = function(member)
					return member.agentId
				end,
				w_full = true,
				gap = "$space.2",
			}),
		}),
		ui.show(function()
			local r = current()
			return not r.loading and not r.error and #(r.members or {}) == 0
		end, ui.text({ "No agents in this group", color = "$text.secondary" })),
		button("Refresh roster", function()
			keiki.roster(M.context:peek().group)
		end),
		button("End group work", function()
			confirm(
				"endGroup",
				"End group work?",
				"End running threads and cancel background tasks in this group. The group and agents are not deleted."
			)
		end),
	})
end
local function tasks(): Node
	local items = ui.computed(function()
		return (keiki.conversation:get() or {}).tasks or {}
	end)
	return ui.column({
		gap = "$space.3",
		w_full = true,
		ui.show(function()
			return #((keiki.conversation:get() or {}).tasks or {}) == 0
		end, ui.text({ "No background tasks", color = "$text.secondary" })),
		ui.each(items, function(task: any, _i: number, now: State<any>): Node
			return ds.Surface({
				kind = "sunken",
				p = "$space.3",
				ui.column({
					w_full = true,
					gap = "$space.2",
					ui.text({
						function()
							return now:get().subagent or "Background task"
						end,
						weight = "$font.weight.medium",
					}),
					ui.text({
						function()
							return now:get().request or ""
						end,
						wrap = true,
						color = "$text.secondary",
					}),
					ds.Badge({
						function()
							return now:get().status or "unknown"
						end,
						tone = "neutral",
						size = "sm",
					}),
					ui.show(
						function()
							local status = now:get().status
							return status == "running" or status == "suspended"
						end,
						button("Cancel task", function()
							M.context:set_key("task", task.id)
							confirm(
								"cancelTask",
								"Cancel background task?",
								"Stop this task without ending the conversation."
							)
						end)
					),
				}),
			})
		end, {
			key = function(task)
				return task.id
			end,
			w_full = true,
			gap = "$space.2",
		}),
	})
end
local function creation(): Node
	local template, name, line = ui.use_state(""), ui.use_state(""), ui.use_state("")
	return ui.column({
		gap = "$space.3",
		w_full = true,
		ui.text({ "Choose a Keiki template. Credentials stay in Keiki.", wrap = true, color = "$text.secondary" }),
		ui.show(M.loading, ui.text({ "Loading templates…", color = "$text.secondary" })),
		ds.Select({
			label = "Agent template",
			value = template,
			options = function()
				local rows = {}
				for _, t in M.templates:get() do
					table.insert(rows, { value = t.id, label = `{t.emoji or ""} {t.name}` })
				end
				return rows
			end,
		}),
		ui.text({
			function()
				for _, t in M.templates:get() do
					if t.id == template:get() then
						return t.blurb or t.description or ""
					end
				end
				return ""
			end,
			wrap = true,
			color = "$text.secondary",
		}),
		ds.TextField({ label = "Agent name (optional)", value = name }),
		ds.TextField({ label = "Line number (optional)", value = line }),
		ds.Button({
			label = "Create agent",
			tone = "primary",
			size = "sm",
			disabled = function()
				return M.busy:get() or template:get() == ""
			end,
			on_click = function()
				call("CreateKeikiAgent", {
					template = template:peek(),
					name = if name:peek() ~= "" then name:peek() else nil,
					lineNumber = if line:peek() ~= "" then line:peek() else nil,
				}, function(answer)
					M.open:set(false)
					keiki.follow()
					local missing = {}
					for _, secret in answer.missing_secrets or {} do
						table.insert(missing, secret.name)
					end
					say(
						if #missing > 0
							then `Agent created. Configure in Keiki: {table.concat(missing, ", ")}`
							else "Agent created"
					)
				end)
			end,
			ui.text({ "Create agent" }),
		}),
		ui.show(function()
			return not M.loading:get() and #M.templates:get() == 0
		end, ui.text({ "No templates available", color = "$text.secondary" })),
		button("Retry templates", M.create),
	})
end
function M.dialog(): Node
	return ds.Dialog({
		open = M.open,
		title = function()
			local c = M.context:get()
			return c.title
				or (
					if c.kind == "roster"
						then c.name .. " · roster"
						elseif c.kind == "agent" then c.name .. " · workspace"
						elseif c.kind == "tasks" then "Background tasks"
						elseif c.kind == "create" then "Create Keiki agent"
						elseif c.kind == "rename" then "Rename local entry"
						else "Conversation actions"
				)
		end,
		cancel = "Done",
		hold = function()
			return M.busy:get()
		end,
		build = function(): Node
			return ui.column({
				w_full = true,
				gap = "$space.3",
				ui.presence(function()
					return M.context:get().kind
				end, function(): Node
					local c = M.context:peek()
					local content: Node
					if c.kind == "roster" then
						content = roster()
					elseif c.kind == "tasks" then
						content = tasks()
					elseif c.kind == "create" then
						content = creation()
					elseif c.kind == "rename" then
						local title = ui.use_state((M.info:peek() or {}).title or "")
						content = ui.column({
							gap = "$space.3",
							ds.TextField({ label = "Local conversation title", value = title }),
							button("Save title", function()
								action("rename", { title = title:peek() })
							end),
						})
					elseif c.kind == "agent" then
						content = ui.column({
							gap = "$space.3",
							Avatar(c.agent, c.name, 48),
							ui.text({ "Desktop view coming soon", color = "$text.secondary" }),
							ui.text({ "Browser view coming soon", color = "$text.secondary" }),
							ui.text({
								"Open a conversation to use its sandbox terminal.",
								wrap = true,
								color = "$text.secondary",
							}),
							button("New conversation", function()
								M.open:set(false)
								keiki.open(c.agent)
							end),
						})
					elseif
						c.kind == "deleteLocal"
						or c.kind == "end"
						or c.kind == "endGroup"
						or c.kind == "cancelTask"
					then
						content = ui.column({
							gap = "$space.3",
							ui.text({ c.body, wrap = true, color = "$text.secondary" }),
							button("Confirm", function()
								if c.kind == "endGroup" then
									call("EndKeikiGroup", { groupId = c.group, confirmed = true }, function(answer)
										M.open:set(false)
										keiki.roster(c.group)
										keiki.follow()
										say(
											`Ended {answer.threadsEnded or 0} threads and cancelled {answer.tasksCancelled or 0} tasks`
										)
									end)
								elseif c.kind == "cancelTask" then
									call("CancelKeikiTask", { chatId = c.chat, taskId = c.task }, function()
										M.open:set(false)
										keiki.retry_conversation()
										say("Task cancelled")
									end)
								else
									action(c.kind)
								end
							end, "danger"),
						})
					else
						content = conversation()
					end
					return content
				end),
				ui.show(M.busy, ui.text({ "Working…", color = "$text.secondary" })),
				ui.show(
					function()
						return M.error:get() ~= nil
					end,
					ui.text({
						function()
							return M.error:get() or ""
						end,
						wrap = true,
						color = "$text.secondary",
					})
				),
			})
		end,
	})
end
function M.toolbar(chat: () -> string?): Node
	return ui.show(
		function()
			return keiki.bound(chat())
		end,
		ui.row({
			w_full = true,
			gap = "$space.2",
			px = "$space.4",
			py = "$space.2",
			items = "center",
			button("Conversation actions", function()
				M.conversation(nil, nil, chat())
			end),
			button("Background tasks", function()
				local id = chat()
				if id then
					M.tasks(id)
				end
			end),
			button("Sandbox terminal", function()
				require("@app/screens/panel").show_terminal(chat())
			end),
		})
	)
end
ui.effect(function()
	local _ = keiki.revision:get()
	serial += 1
	M.open:set(false)
	M.busy:set(false)
	M.error:set(nil)
	M.templates:set({})
end)
return M
