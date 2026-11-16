--!strict
-- Rendering of the `seiji.app` block: a head row, a strip of numbers, the Devices and Runs tabs,
-- and a dock with the flash, the confirm strip and the key legend. Pure functions of the state
-- built in app.luau (passed in as `A` to avoid a require cycle).

local cli = require("./cli")
local fmt = require("./fmt")
local h = require("./ui")

local ui = tern.ui

local V = {}

local function key_of(id: string): string
	return "k" .. (string.gsub(id, "[^%w]", "_"))
end

local function host_of(url: string): string
	return string.match(url, "^%a+://([^/]+)") or url
end

local function head(state: any): Node
	local meta = "  not connected"
	local c = state.api.creds
	if c then
		local b = state.data.budget
		local who = (type(b) == "table" and b.user) or c.user or "?"
		meta = "  " .. host_of(c.url) .. " · " .. who
	end
	local right: { Node } = {}
	if c then
		table.insert(right, h.action_key("New device", "new", "n", "accent"))
	end
	table.insert(right, h.action("Refresh", "refresh", "neutral"))
	return ui.node("row", { key = "head", justify = "between", align = "center" }, {
		ui.text({ ui.span("Seiji", "title strong"), ui.span(meta, "muted") }),
		h.row("end", right),
	})
end

local function strip(state: any): Node?
	local b = state.data.budget
	if type(b) ~= "table" then
		return nil
	end
	local used, quota = b.used_minutes or 0, b.monthly_minutes or 0
	local share = if quota > 0 then used / quota else 0
	local tone = if share >= 0.9 then "strong error" elseif share >= 0.75 then "strong warning" else "strong"
	local n = #(b.active_leases or {})
	return h.strip("strip", {
		{ "Minutes in " .. tostring(b.month or "this month"), { ui.span(fmt.num(used), tone), ui.span(" / " .. fmt.num(quota), "muted") } },
		{ "Left", { ui.span(fmt.num(b.remaining_minutes), "strong"), ui.span(" min", "muted") } },
		{ "Devices", { ui.span(tostring(n), "strong"), ui.span(" / " .. tostring(b.max_concurrent or "?"), "muted") } },
		{ "Cost", { ui.span(if b.estimated_cost_usd ~= nil then fmt.money(b.estimated_cost_usd) else "—", "strong mono") } },
	})
end

local function nav(state: any, A: any): Node
	local n_dev = #A.devices(state)
	local n_run = #A.runs(state)
	local function lab(text: string, n: number): Spans
		if n > 0 then
			return { ui.span(text .. " "), ui.span(tostring(n), "muted") }
		end
		return text
	end
	return ui.node("tabs", {
		key = "nav",
		active = state.view,
		items = { { id = "devices", label = lab("Devices", n_dev) }, { id = "runs", label = lab("Runs", n_run) } },
	})
end

-- A pickable list: a click selects (`sel=<id>`), a double click runs `open`.
local function pick_list(key: string, items: { any }, selected: string?, dbl: string): Node
	local rows: { Item } = {}
	for i, it in items do
		rows[i] = { label = it.label, detail = it.detail, value = it.value, icon = it.icon, tone = it.tone }
	end
	local list = ui.list(rows)
	local sel_node = nil
	for i, child in list.c or {} do
		local id = items[i].id
		local k = key_of(id)
		h.with(child, { key = k, actions = { click = "sel=" .. id, dblclick = dbl } })
		if id == selected then
			sel_node = "main.body.list." .. key .. "." .. k
		end
	end
	list.p = { key = key, selected = sel_node }
	return list
end

-- Below this width (cells) the detail card goes under the list: with the screen open beside the
-- panel, side by side would squeeze the list to a few letters.
local WIDE_COLS = 120

local function master_detail(state: any, list: Node, detail: Node): Node
	if (state.cols or 200) < WIDE_COLS then
		return ui.node("col", { key = "body", gap = "lg" }, {
			ui.node("col", { key = "list" }, { list }),
			detail,
		})
	end
	h.with(detail, { min = { w = "44ch" }, max = { w = "60ch" }, basis = "content" })
	return ui.node("row", { key = "body", gap = "lg", align = "start" }, {
		ui.node("col", { key = "list", grow = 1, basis = 0 }, { list }),
		detail,
	})
end

-- ── Devices ────────────────────────────────────────────────────────────────

local STATUS_TONE = { ready = "success", booting = "pending" }

local function device_actions(state: any, d: any): Node
	local acts: { Node } = {}
	local ready = d.status == "ready"
	if d.platform == "ios" and ready then
		local opening = state.busy["viewer:" .. d.lease_id]
		table.insert(acts, h.action_key(if opening then "Opening…" else "Open screen", "open", "o", "accent"))
		table.insert(acts, h.action_key("Float", "float", "f"))
	end
	if ready then
		table.insert(acts, h.action_key(if state.busy["shot:" .. d.lease_id] then "Capturing…" else "Screenshot", "shot", "s"))
	end
	if d.platform == "ios" and ready then
		table.insert(acts, h.action_key("Share link", "share", "l"))
	end
	table.insert(acts, h.action_key(if state.busy["release:" .. d.lease_id] then "Releasing…" else "Release", "release", "x", "error"))
	return ui.node("row", { key = "acts", gap = "sm", align = "center", wrap = true }, acts)
end

local function device_detail(state: any, d: any): Node
	local body: { Node } = {
		h.with(ui.kv({
			{ "Platform", if d.platform == "ios" then "iOS simulator" else "Android emulator" },
			{ "Device", { ui.span(tostring(d.device_id or "—"), "mono") } },
			{ "Lease", { ui.span(d.lease_id, "mono") } },
			{ "Started", fmt.ago(d.started_at) },
			{ "Released", "at the latest " .. fmt.until_(d.ends_at_latest) .. ", or after " .. tostring(d.idle_release_minutes) .. " idle min" },
		}), { key = "kv" }),
		device_actions(state, d),
	}
	local shot = state.shots[d.lease_id]
	if shot then
		table.insert(body, h.muted("Screenshot " .. fmt.ago(shot.at) .. " · click to zoom"))
		table.insert(body, ui.node("image", { key = "shot", blob = shot.blob, alt = "Screenshot of " .. d.model, max = { h = "16lines" } }))
	end
	return h.card("detail", { ui.span(d.model, "strong"), ui.span("  " .. d.status, STATUS_TONE[d.status] or "muted") }, body)
end

local function devices_view(state: any, A: any): Node
	local list = A.devices(state)
	local items = {}
	if state.booting then
		local secs = (tern.now() - state.booting.at) // 1000
		table.insert(items, {
			id = "booting",
			label = { ui.span(state.booting.model, "strong") },
			detail = { ui.span("  booting · " .. secs .. " s (usually 30-90 s)", "pending") },
			icon = "clock",
			tone = "pending",
		})
	end
	for _, d in list do
		table.insert(items, {
			id = d.lease_id,
			label = { ui.span(d.model, "strong"), ui.span("  " .. (if d.platform == "ios" then "iOS" else "Android"), "muted") },
			detail = { ui.span("  " .. d.lease_id, "muted mono") },
			value = { ui.span(d.status, STATUS_TONE[d.status] or "muted"), ui.span(" · ends " .. fmt.until_(d.ends_at_latest), "muted") },
			icon = if d.status == "ready" then "monitor" else "clock",
			tone = STATUS_TONE[d.status],
		})
	end
	if #list == 0 then
		if state.booting then
			return h.col("body", { pick_list("devices", items, nil, "open") })
		end
		return h.empty("body", "No devices", "Boot a simulator for yourself. It counts against your minutes until you release it.", h.action_key("New device", "new", "n", "accent"))
	end
	local d = A.current(state)
	return master_detail(state, pick_list("devices", items, d and d.lease_id, "open"), device_detail(state, d))
end

-- ── Runs ───────────────────────────────────────────────────────────────────

local RUN_TONE = { passed = "success", failed = "error", running = "accent", error = "error" }
local RUN_ICON = { passed = "check", failed = "x-circle", running = "activity", error = "warn" }

local function run_detail(state: any, r: any): Node
	local body: { Node } = {
		ui.node("row", { key = "acts", gap = "sm", align = "center" }, {
			h.action_key("Copy summary", "copy", "c", "accent"),
			h.action_key(if state.busy["zip:" .. r.run_id] then "Downloading…" else "Download evidence", "evidence", "d"),
		}),
	}
	local rep = state.reports[r.run_id]
	if not rep then
		table.insert(body, h.loading("loading", "Loading the summary"))
	elseif rep.error then
		table.insert(body, ui.text({ ui.span(rep.error, "error") }))
	else
		table.insert(body, h.with(ui.md(rep.md), { key = "md" }))
	end
	local title = r.name or r.kind
	return h.card("detail", { ui.span(title, "strong"), ui.span("  " .. r.status, RUN_TONE[r.status] or "muted") }, body)
end

local function runs_view(state: any, A: any): Node
	local list = A.runs(state)
	if #list == 0 then
		return h.empty("body", "No runs yet", "Agents record runs with run_actions, run_xctest or run_flow. Each one keeps its video, log and screenshots.")
	end
	local items = {}
	for _, r in list do
		table.insert(items, {
			id = r.run_id,
			label = { ui.span(r.name or r.kind, "strong"), ui.span("  " .. r.kind, "muted") },
			detail = { ui.span("  " .. (r.verdict or r.run_id), "muted") },
			value = { ui.span(r.status, RUN_TONE[r.status] or "muted"), ui.span(" · " .. fmt.ago(r.started_at), "muted") },
			icon = RUN_ICON[r.status] or "reel",
			tone = RUN_TONE[r.status],
		})
	end
	local r = A.current(state)
	return master_detail(state, pick_list("runs", items, r and r.run_id, "copy"), run_detail(state, r))
end

-- ── signed out, errors ─────────────────────────────────────────────────────

local function connection(state: any): Node?
	local api = state.api
	if api.auth == "ok" then
		local err = state.errs.budget
		if err and not state.data.budget then
			return h.error_card("body", "Could not reach Seiji", err, "retry")
		end
		if not state.data.budget then
			return h.loading("body", "Connecting to Seiji")
		end
		return nil
	end
	if api.auth == "checking" then
		return h.loading("body", "Reading your Seiji sign-in")
	elseif api.auth == "logged_out" then
		return h.empty("body", "Sign in to Seiji", "Paste the token your Seiji admin gave you. It stays in the macOS keychain.", h.action_key("Sign in", "signin", "⏎", "accent"))
	end
	return h.error_card("body", if api.auth == "cli_missing" then "The seiji CLI is missing" else "Could not read your sign-in", cli.describe(api.auth_error), "retry")
end

local LEGENDS = {
	devices = { { "tab", "runs" }, { "↑↓", "device" }, { "o", "open screen" }, { "f", "float" }, { "s", "screenshot" }, { "l", "share" }, { "x", "release" }, { "n", "new" } },
	runs = { { "tab", "devices" }, { "↑↓", "run" }, { "c", "copy summary" }, { "d", "evidence" }, { "r", "refresh" } },
}

function V.render(state: any, A: any): BlockView
	local body = connection(state)
	local parts: { Node } = { head(state) }
	if not body then
		local s = strip(state)
		if s then
			table.insert(parts, s)
		end
		table.insert(parts, nav(state, A))
		body = if state.view == "runs" then runs_view(state, A) else devices_view(state, A)
	end
	table.insert(parts, body :: Node)
	local dock: { Node } = {}
	if state.flash then
		table.insert(dock, h.with(ui.text({ ui.span(state.flash.text, state.flash.tone) }), { key = "flash" }))
	end
	if state.confirm then
		table.insert(dock, h.confirm(state.confirm.title, state.confirm.line, state.confirm.ok, state.confirm.tone))
	end
	if state.api.auth == "ok" then
		table.insert(dock, h.legend(LEGENDS[state.view] or LEGENDS.devices))
	elseif state.api.auth == "logged_out" then
		table.insert(dock, h.legend({ { "enter", "sign in" }, { "r", "check again" } }))
	else
		table.insert(dock, h.legend({ { "r", "try again" } }))
	end
	return { main = ui.node("col", { gap = "lg" }, parts), dock = ui.col(dock) }
end

return V
