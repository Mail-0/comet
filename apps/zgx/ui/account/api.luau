-- Keiki organizations, mediated by the engine's account service.
local M = {}

export type Org = { id: string, organization_id: string, name: string }

function M.orgs(): any
	return ui.call("account.orgs")
end

function M.choose_org(organization_id: string): any
	return ui.call("account.choose_org", organization_id)
end

local SAYS: { [string]: string } = {
	offline = "account.api.says.offline",
	signed_out = "account.api.says.signed_out",
	invalid = "account.api.says.invalid",
	refused = "account.api.says.refused",
	failed = "account.api.says.failed",
}

function M.says(word: string?): string
	return ui.t(SAYS[word or "failed"] or SAYS.failed):get()
end

return M
