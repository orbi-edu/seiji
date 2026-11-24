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
	local unlimited = b.monthly_unlimited == true
	local used, quota = b.used_minutes or 0, b.monthly_minutes or 0
	local share = if not unlimited and quota > 0 then used / quota else 0
	local tone = if unlimited then "strong"
		elseif share >= 0.9 then "strong error"
		elseif share >= 0.75 then "strong warning" else "strong"
	local n = #(b.active_leases or {})
	local left_spans
	if unlimited then
		left_spans = { ui.span("Unlimited", "strong accent") }
	else
		left_spans = { ui.span(fmt.num(b.remaining_minutes), "strong"), ui.span(" min", "muted") }
	end
	return h.strip("strip", {
		{ "Minutes in " .. tostring(b.month or "this month"), { ui.span(fmt.num(used), tone), ui.span(" / " .. (if unlimited then "∞" else fmt.num(quota)), "muted") } },
		{ "Left", left_spans },
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
	local tabs = {
		{ id = "devices", label = lab("Devices", n_dev) },
		{ id = "runs", label = lab("Runs", n_run) },
	}
	if state.is_admin then
		local n_usr = #A.admin_users(state)
		table.insert(tabs, { id = "users", label = lab("Users", n_usr) })
	end
	return ui.node("tabs", {
		key = "nav",
		active = state.view,
		items = tabs,
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

-- ── Users (admin) ─────────────────────────────────────────────────────────

-- The devices the user holds, each with Watch (view-only), Control and Copy link (view-only).
local function user_devices(state: any, A: any, u: any): Node
	local rows: { Node } = {}
	for _, d in A.admin_devices(state, u.id) do
		local parts: { Node } = {
			ui.text({
				ui.span(d.model, "strong"),
				ui.span("  " .. (if d.platform == "ios" then "iOS" else "Android"), "muted"),
				ui.span("  " .. d.status, STATUS_TONE[d.status] or "muted"),
				ui.span("  " .. d.lease_id, "muted mono"),
				ui.span(" · ends " .. fmt.until_(d.ends_at_latest), "muted"),
			}),
		}
		if d.platform == "ios" and d.status == "ready" then
			local opening = state.busy["viewer:" .. d.lease_id]
			table.insert(parts, h.action(if opening then "Opening…" else "Watch", "admin_watch=" .. d.lease_id, "accent", "watch"))
			table.insert(parts, h.action("Control", "admin_control=" .. d.lease_id, "neutral", "control"))
			table.insert(parts, h.action("Copy link", "admin_link=" .. d.lease_id, "neutral", "link"))
		end
		table.insert(rows, ui.node("row", { key = key_of(d.lease_id), gap = "sm", align = "center", wrap = true }, parts))
	end
	local err = state.errs.admin_leases
	local loaded = type(state.data.admin_leases) == "table"
	if #rows == 0 then
		-- "No devices" only after a list came back; a failed first read says why instead.
		if loaded then
			table.insert(rows, h.with(h.muted("No devices."), { key = "none" }))
		elseif err then
			table.insert(rows, h.with(ui.text({ ui.span("Could not load devices: " .. err, "error") }), { key = "err" }))
		else
			table.insert(rows, h.with(h.muted("Loading devices…"), { key = "loading" }))
		end
	end
	if loaded and err then
		table.insert(rows, h.with(h.muted("Could not refresh: " .. err), { key = "stale" }))
	end
	return h.card("devices", { ui.span("Devices", "strong") }, rows)
end

local function user_detail(state: any, A: any, u: any): Node
	local is_editing = state.editing and state.editing.user_id == u.id
	local body: { Node } = {}

	-- Read-only fields.
	local kv_rows = {
		{ "Status", if u.disabled then "disabled" else "active" },
		{ "Admin", if u.is_admin then "yes" else "no" },
		{ "Active devices", tostring(u.active_devices or 0) },
		{ "Used minutes", fmt.num(u.used_minutes) },
		{ "Session limit", tostring(u.max_lease_minutes or "—") .. " min" },
		{ "Idle release", tostring(u.idle_minutes or "—") .. " min" },
	}
	table.insert(body, h.with(ui.kv(kv_rows), { key = "kv" }))

	if is_editing then
		local ed = state.editing
		-- Editable fields rendered as key-value with action badges for changes.
		local edit_rows: { Node } = {}

		-- Max devices (editable with +/- steppers).
		local cur_conc = tonumber(ed.max_concurrent) or 1
		table.insert(edit_rows, ui.node("row", { key = "ed_conc", gap = "sm", align = "center" }, {
			ui.text({ ui.span("Max devices  ", "muted") }),
			h.action("−", "edit_field=" .. tern.json.encode({ field = "max_concurrent", value = tostring(math.max(1, cur_conc - 1)) }), "neutral", "dec_conc"),
			ui.text({ ui.span(" " .. tostring(ed.max_concurrent) .. " ", "strong mono") }),
			h.action("+", "edit_field=" .. tern.json.encode({ field = "max_concurrent", value = tostring(cur_conc + 1) }), "neutral", "inc_conc"),
		}))
		if cur_conc < (u.max_concurrent or 1) then
			table.insert(edit_rows, h.muted("Lowering max devices blocks new acquisitions but does not evict held devices."))
		end

		-- Unlimited monthly budget toggle (before budget steppers so the user decides first).
		local ulabel = if ed.monthly_unlimited then "Unlimited monthly budget  ✓" else "Unlimited monthly budget"
		local utone = if ed.monthly_unlimited then "accent" else "neutral"
		local unlim_children: { Node } = {
			h.action(ulabel, "edit_field=" .. tern.json.encode({ field = "monthly_unlimited", value = not ed.monthly_unlimited }), utone, "tog_unlim"),
		}
		if ed.monthly_unlimited then
			table.insert(unlim_children, ui.text({ ui.span("  Budget is not enforced; minutes are still metered.", "muted") }))
		end
		table.insert(edit_rows, ui.node("row", { key = "ed_unlim", gap = "sm", align = "center" }, unlim_children))

		-- Monthly minutes (saved numeric budget, shown read-only while unlimited, editable otherwise).
		local cur_mins = tonumber(ed.monthly_minutes) or 0
		if ed.monthly_unlimited then
			table.insert(edit_rows, ui.node("row", { key = "ed_mins", gap = "sm", align = "center" }, {
				ui.text({ ui.span("Saved budget  ", "muted"), ui.span(ed.monthly_minutes .. " min", "strong mono"), ui.span("  (preserved for switching back)", "muted") }),
			}))
		else
			local function mins_action(delta: number): Node
				local new_val = math.max(0, cur_mins + delta)
				local label = (if delta > 0 then "+" else "") .. tostring(delta)
				return h.action(label, "edit_field=" .. tern.json.encode({ field = "monthly_minutes", value = tostring(new_val) }), "neutral", "mins_" .. tostring(delta))
			end
			table.insert(edit_rows, ui.node("row", { key = "ed_mins", gap = "sm", align = "center", wrap = true }, {
				ui.text({ ui.span("Monthly minutes  ", "muted") }),
				mins_action(-600),
				mins_action(-60),
				ui.text({ ui.span(" " .. ed.monthly_minutes .. " ", "strong mono") }),
				mins_action(60),
				mins_action(600),
			}))
			if cur_mins < (u.monthly_minutes or 0) then
				table.insert(edit_rows, h.muted("Reducing the budget can end leases on the next reaper pass."))
			end
		end

		-- Error message.
		if state.edit_error then
			table.insert(edit_rows, ui.text({ ui.span(state.edit_error, "error") }))
		end

		-- Save / Cancel.
		table.insert(edit_rows, ui.node("row", { key = "ed_acts", gap = "sm", align = "center" }, {
			h.action_key(if state.saving then "Saving…" else "Save", "save_edit", "⏎", "accent"),
			h.action_key("Cancel", "cancel_edit", "esc", "muted"),
		}))

		table.insert(body, h.card("edit", { ui.span("Edit limits", "strong") }, edit_rows))
	else
		-- Show current limits and an Edit action.
		local limit_rows = {
			{ "Max devices", tostring(u.max_concurrent or 1) },
			{ "Monthly minutes", tostring(u.monthly_minutes or 0) },
			{ "Monthly budget", if u.monthly_unlimited then "Unlimited" else fmt.num((u.monthly_minutes or 0) - (u.used_minutes or 0)) .. " min left" },
		}
		table.insert(body, h.with(ui.kv(limit_rows), { key = "lim" }))
		table.insert(body, ui.node("row", { key = "acts", gap = "sm", align = "center" }, {
			h.action_key("Edit", "edit_user", "e", "accent"),
		}))
	end
	table.insert(body, user_devices(state, A, u))

	local name_spans: Spans = { ui.span(u.name, "strong") }
	if u.disabled then
		table.insert(name_spans, ui.span("  disabled", "error"))
	end
	if u.is_admin then
		table.insert(name_spans, ui.span("  admin", "accent"))
	end
	return h.card("detail", name_spans, body)
end

local function users_view(state: any, A: any): Node
	local list = A.admin_users(state)
	local err = state.errs.admin_users
	if err and #list == 0 then
		return h.error_card("body", "Could not load users", err, "refresh")
	end
	if #list == 0 then
		return h.loading("body", "Loading users")
	end

	-- Host capacity strip.
	local cap = state.data.capacity
	local inuse = state.data.in_use
	local cap_parts: { Node } = {}
	if type(cap) == "table" then
		local ios_cap = cap.ios or 0
		local ios_used = if type(inuse) == "table" then inuse.ios or 0 else 0
		table.insert(cap_parts, ui.text({ ui.span("Host capacity  ", "muted"), ui.span("iOS " .. tostring(ios_used) .. "/" .. tostring(ios_cap), "strong") }))
		if (cap.android or 0) > 0 then
			local and_used = if type(inuse) == "table" then inuse.android or 0 else 0
			table.insert(cap_parts, ui.text({ ui.span("  Android " .. tostring(and_used) .. "/" .. tostring(cap.android), "strong") }))
		end
	end

	local items = {}
	for _, u in list do
		local budget_text
		if u.monthly_unlimited then
			budget_text = "Unlimited"
		else
			local rm = u.monthly_minutes - (u.used_minutes or 0)
			budget_text = fmt.num(rm) .. " min left"
		end
		local label_spans: { any } = { ui.span(u.name, "strong") }
		if u.is_admin then
			table.insert(label_spans, ui.span("  admin", "accent"))
		end
		if u.disabled then
			table.insert(label_spans, ui.span("  disabled", "error"))
		end
		table.insert(items, {
			id = tostring(u.id),
			label = label_spans,
			detail = { ui.span("  " .. tostring(u.active_devices or 0) .. "/" .. tostring(u.max_concurrent or 1) .. " devices", "muted") },
			value = { ui.span(budget_text, if u.monthly_unlimited then "accent" else "muted") },
			icon = if u.disabled then "user-x" else "user",
			tone = if u.disabled then "error" else nil,
		})
	end

	local u = A.admin_current(state)
	local sel_id = if u then tostring(u.id) else nil
	local detail_parts: { Node } = {}
	if #cap_parts > 0 then
		table.insert(detail_parts, ui.node("row", { key = "cap", gap = "sm", align = "center" }, cap_parts))
	end
	table.insert(detail_parts, user_detail(state, A, u))
	local detail = ui.node("col", { key = "detail", gap = "md" }, detail_parts)
	return master_detail(state, pick_list("users", items, sel_id, "edit_user"), detail)
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

local LEGENDS_DEVICES = { { "tab", "runs" }, { "↑↓", "device" }, { "o", "open screen" }, { "f", "float" }, { "s", "screenshot" }, { "l", "share" }, { "x", "release" }, { "n", "new" } }
local LEGENDS_RUNS = { { "tab", "devices" }, { "↑↓", "run" }, { "c", "copy summary" }, { "d", "evidence" }, { "r", "refresh" } }
local LEGENDS_RUNS_ADMIN = { { "tab", "users" }, { "↑↓", "run" }, { "c", "copy summary" }, { "d", "evidence" }, { "r", "refresh" } }
local LEGENDS_USERS = { { "tab", "devices" }, { "↑↓", "user" }, { "e", "edit" }, { "⏎", "save" }, { "esc", "cancel" }, { "r", "refresh" } }

local function legend_for(state: any): { { string } }
	if state.view == "users" then
		return LEGENDS_USERS
	elseif state.view == "runs" then
		return if state.is_admin then LEGENDS_RUNS_ADMIN else LEGENDS_RUNS
	end
	return LEGENDS_DEVICES
end

function V.render(state: any, A: any): BlockView
	local body = connection(state)
	local parts: { Node } = { head(state) }
	if not body then
		local s = strip(state)
		if s then
			table.insert(parts, s)
		end
		table.insert(parts, nav(state, A))
		body = if state.view == "runs" then runs_view(state, A)
			elseif state.view == "users" then users_view(state, A)
			else devices_view(state, A)
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
		table.insert(dock, h.legend(legend_for(state)))
	elseif state.api.auth == "logged_out" then
		table.insert(dock, h.legend({ { "enter", "sign in" }, { "r", "check again" } }))
	else
		table.insert(dock, h.legend({ { "r", "try again" } }))
	end
	return { main = ui.node("col", { gap = "lg" }, parts), dock = ui.col(dock) }
end

return V
