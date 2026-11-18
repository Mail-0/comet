--!strict
type Entry = { id: string, name: string? }
type Account = {
    signed_in: boolean,
    busy: boolean,
    orgs: { Entry }?,
    active_org: string?,
    agents: { Entry },
    error: string?,
}
return ui.shared("keiki.account", { signed_in = false, busy = false, agents = {} } :: Account)
