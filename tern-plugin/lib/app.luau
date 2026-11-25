--!strict
-- Block `seiji.app`: your budget, the devices you hold and your recorded runs.
--
-- Reads go through MCP tools (`budget_status`, `list_runs`, `export_evidence`, …) with the token
-- the CLI hands over. Opening a device's screen asks the server for a one-time viewer link and
-- opens it through the window half (`seiji://ui?req=…`), which shows it in Tern's browser beside
-- this block, or floating as a picture in picture.

local apimod = require("./api")
local cli = require("./cli")
local fmt = require("./fmt")
local V = require("./views")

local M = {}

local POLL_MS = 3000
local RUNS_MS = 10000
local AUTH_RETRY_MS = 5000
local FLASH_MS = 6000
-- Booting takes 30-90 s; the tool answers when the device is ready.
local ACQUIRE_MS = 180000

local function running(pane: number): boolean
	for _, p in tern.pane.list() do
		if p.pane == pane then
			return true
		end
	end
	return false
end

local function flash(state: any, tone: string, text: string)
	state.flash = { tone = tone, text = text, at = tern.now() }
end

local function fail(state: any, what: string, err: cli.CliError?)
	flash(state, "error", what .. ": " .. cli.describe(err))
end

-- Opens a window-only UI (picker, browser, new tab) for this pane.
local function ask(state: any, req: { [string]: any })
	req.pane = state.cx.pane
	state.cx:open(fmt.ui_link(req))
end

-- ── reads ──────────────────────────────────────────────────────────────────

local poll: (state: any) -> ()
local read_admin_users: (state: any) -> ()

local function read(state: any, name: string, tool: string, args: { [string]: any }?)
	if state.inflight[name] then
		return
	end
	state.inflight[name] = true
	apimod.tool(state.api, tool, args, nil, function(value, err)
		state.inflight[name] = nil
		if state.stopped then
			return
		end
		state.read_at[name] = tern.now()
		if err then
			local text = cli.describe(err)
			if state.errs[name] ~= text then
				state.errs[name] = text
				state.cx:render()
			end
			return
		end
		local was = state.raw[name]
		local now = tern.json.encode(value)
		state.errs[name] = nil
		state.data[name] = value
		state.raw[name] = now
		state.updated = tern.now()
		if was ~= now then
			state.cx:render()
		end
	end)
end

local function devices(state: any): { any }
	local b = state.data.budget
	return if type(b) == "table" and type(b.active_leases) == "table" then b.active_leases else {}
end

local function runs(state: any): { any }
	local r = state.data.runs
	return if type(r) == "table" then r else {}
end

M.devices = devices
M.runs = runs

local function find(list: { any }, field: string, id: string?): any
	for _, it in list do
		if it[field] == id then
			return it
		end
	end
	return nil
end

-- The selected device or run; the first one when the selection is gone.
local function current(state: any): any
	if state.view == "runs" then
		local list = runs(state)
		local r = find(list, "run_id", state.sel.runs) or list[1]
		state.sel.runs = r and r.run_id
		return r
	end
	local list = devices(state)
	local d = find(list, "lease_id", state.sel.devices) or list[1]
	state.sel.devices = d and d.lease_id
	return d
end

M.current = current

local function load_report(state: any, run: any)
	if not run or state.reports[run.run_id] or state.inflight["report:" .. run.run_id] then
		return
	end
	local id = run.run_id
	state.inflight["report:" .. id] = true
	apimod.tool(state.api, "export_evidence", { run_id = id }, nil, function(md, err)
		state.inflight["report:" .. id] = nil
		if state.stopped then
			return
		end
		state.reports[id] = if err then { error = cli.describe(err) } else { md = tostring(md) }
		state.cx:render()
	end)
end

-- Revoke admin: clear cached admin data, editing, and return to devices if on users view.
local function clear_admin(state: any)
	state.is_admin = false
	state.data.admin_users = nil
	state.data.admin_leases = nil
	state.data.admin_viewer = nil
	state.data.capacity = nil
	state.data.in_use = nil
	state.errs.admin_users = nil
	state.errs.admin_leases = nil
	state.errs.admin_viewer = nil
	state.read_at.admin_users = nil
	state.read_at.admin_leases = nil
	state.read_at.admin_viewer = nil
	state.editing = nil
	state.edit_error = nil
	state.saving = false
	if state.view == "users" then
		state.view = "devices"
	end
end

poll = function(state: any)
	local api = state.api
	if api.auth ~= "ok" then
		if not api.authing and tern.now() - api.auth_at >= AUTH_RETRY_MS then
			apimod.authenticate(api, function()
				if state.stopped then
					return
				end
				state.cx:render()
				if api.auth == "ok" then
					poll(state)
				end
			end)
		end
		return
	end
	-- Detect admin; re-check on every auth cycle (is_admin == nil).
	if state.is_admin == nil and not state.inflight.me then
		state.inflight.me = true
		apimod.me(api, function(data, err)
			state.inflight.me = nil
			if state.stopped then
				return
			end
			if err then
				-- Transient error: leave nil so the next poll retries.
				state.is_admin = nil
				-- A non-admin on the users view gets bounced.
				if state.view == "users" then
					state.view = "devices"
				end
			elseif type(data) == "table" then
				local was_admin = state.is_admin
				state.is_admin = data.is_admin == true
				if state.is_admin and state.pending_view == "users" then
					state.view = "users"
					state.pending_view = nil
				end
				if not state.is_admin then
					state.pending_view = nil
					if was_admin ~= false then
						clear_admin(state)
					end
				end
			else
				state.is_admin = false
				state.pending_view = nil
				clear_admin(state)
			end
			state.cx:render()
			-- Kick another poll to load data now that admin is known.
			if state.is_admin ~= nil then
				poll(state)
			end
		end)
		return
	end
	read(state, "budget", "budget_status")
	local last = state.read_at.runs
	if state.view == "runs" or not last then
		if not last or tern.now() - last >= RUNS_MS then
			read(state, "runs", "list_runs", { limit = 30 })
		end
	end
	if state.view == "runs" then
		load_report(state, current(state))
	end
	if state.view == "users" and state.is_admin then
		local ulast = state.read_at.admin_users
		if not ulast or tern.now() - ulast >= RUNS_MS then
			read_admin_users(state)
		end
	end
end

-- Reads one admin endpoint; `apply` stores its data. A 403 drops the admin UI.
local function read_admin(state: any, name: string, call: (any, (any, cli.CliError?) -> ()) -> (), apply: (data: any) -> ())
	if state.inflight[name] then
		return
	end
	state.inflight[name] = true
	call(state.api, function(data, err)
		state.inflight[name] = nil
		if state.stopped then
			return
		end
		state.read_at[name] = tern.now()
		if err then
			if err.code == "forbidden" then
				clear_admin(state)
				flash(state, "error", "You no longer have admin access.")
			else
				state.errs[name] = cli.describe(err)
			end
			state.cx:render()
			return
		end
		state.errs[name] = nil
		if type(data) == "table" then
			apply(data)
			state.cx:render()
		end
	end)
end

-- The user list and every user's devices.
read_admin_users = function(state: any)
	read_admin(state, "admin_users", apimod.admin_users, function(data)
		state.data.admin_users = data.users
		state.data.capacity = data.capacity
		state.data.in_use = data.in_use
	end)
	read_admin(state, "admin_leases", apimod.admin_leases, function(data)
		state.data.admin_leases = data.leases
	end)
	-- A read that started before a save may answer after it; keep the save's reply then.
	local gen = state.viewer_gen or 0
	read_admin(state, "admin_viewer", apimod.admin_viewer, function(data)
		if (state.viewer_gen or 0) == gen and not state.inflight.set_viewer then
			state.data.admin_viewer = data
		end
	end)
end

-- The viewer settings (`/v1/admin/viewer`), or nil before the first read.
function M.admin_viewer(state: any): any
	return state.data.admin_viewer
end

-- Changes viewer settings on the server; the reply is the new state.
local function set_viewer(state: any, body: { [string]: any })
	if state.inflight.set_viewer then
		return
	end
	state.inflight.set_viewer = true
	state.viewer_gen = (state.viewer_gen or 0) + 1
	apimod.admin_set_viewer(state.api, body, function(data, err)
		state.inflight.set_viewer = nil
		if state.stopped then
			return
		end
		if err then
			if err.code == "forbidden" then
				clear_admin(state)
				flash(state, "error", "You no longer have admin access.")
			else
				flash(state, "error", cli.describe(err))
			end
		elseif type(data) == "table" then
			state.data.admin_viewer = data
			flash(state, "success", "Viewer settings saved. They last until Seiji restarts.")
		end
		state.cx:render()
	end)
end

local function admin_users(state: any): { any }
	local u = state.data.admin_users
	return if type(u) == "table" then u else {}
end

M.admin_users = admin_users

local function admin_leases(state: any): { any }
	local l = state.data.admin_leases
	return if type(l) == "table" then l else {}
end

-- The devices that user `user_id` holds now.
local function admin_devices(state: any, user_id: number): { any }
	local out = {}
	for _, d in admin_leases(state) do
		if d.user_id == user_id then
			table.insert(out, d)
		end
	end
	return out
end

M.admin_devices = admin_devices

local function admin_current(state: any): any
	local list = admin_users(state)
	local u = find(list, "id", state.sel.users) or list[1]
	state.sel.users = u and u.id
	return u
end

M.admin_current = admin_current

-- Start editing a user's limits: copy server values into a draft.
local function start_edit(state: any, u: any)
	if not u then
		return
	end
	state.editing = {
		user_id = u.id,
		monthly_minutes = tostring(u.monthly_minutes or 0),
		monthly_unlimited = u.monthly_unlimited == true,
		max_concurrent = tostring(u.max_concurrent or 1),
	}
end

-- Cancel editing.
local function cancel_edit(state: any)
	state.editing = nil
	state.edit_error = nil
end

-- Save the edited limits to the server.
local function save_edit(state: any)
	local ed = state.editing
	if not ed or state.saving then
		return
	end
	local mins = tonumber(ed.monthly_minutes)
	local conc = tonumber(ed.max_concurrent)
	if not mins or mins < 0 or mins ~= math.floor(mins) then
		state.edit_error = "Monthly minutes must be a non-negative integer."
		state.cx:render()
		return
	end
	if not conc or conc < 1 or conc ~= math.floor(conc) then
		state.edit_error = "Max devices must be at least 1."
		state.cx:render()
		return
	end
	state.saving = true
	state.edit_error = nil
	apimod.admin_update_user(state.api, ed.user_id, {
		monthly_minutes = mins,
		monthly_unlimited = ed.monthly_unlimited,
		max_concurrent = conc,
	}, function(data, err)
		state.saving = false
		if state.stopped then
			return
		end
		if err then
			if err.code == "forbidden" then
				clear_admin(state)
				flash(state, "error", "You no longer have admin access.")
			else
				state.edit_error = cli.describe(err)
			end
			state.cx:render()
			return
		end
		-- Success: merge the updated user into the list and leave edit mode.
		if type(data) == "table" then
			local list = admin_users(state)
			for i, u in list do
				if u.id == data.id then
					list[i] = data
					break
				end
			end
		end
		state.editing = nil
		state.edit_error = nil
		flash(state, "success", "Saved limits for user " .. tostring(ed.user_id) .. ".")
		-- Re-fetch authoritative data.
		state.read_at.admin_users = nil
		read_admin_users(state)
		state.cx:render()
	end)
	state.cx:render()
end

-- ── actions ────────────────────────────────────────────────────────────────

-- Asks for a viewer link to `d`, then opens it beside this panel (`how` "beside"), floating
-- ("float"), or copies it ("copy"). `admin` asks through the admin route, for any user's device.
local function viewer(state: any, d: any, control: boolean, how: string, admin: boolean)
	if not d then
		return
	end
	if d.platform ~= "ios" then
		flash(state, "warning", "The live screen supports iOS simulators only for now.")
		return
	end
	if d.status ~= "ready" then
		flash(state, "warning", "The device is still booting.")
		return
	end
	local key = "viewer:" .. d.lease_id
	if state.busy[key] then
		return
	end
	state.busy[key] = true
	local path = (if admin then "/v1/admin/leases/" else "/v1/leases/") .. d.lease_id .. "/viewer"
	apimod.request(state.api, "POST", path, { body = { control = control } }, function(link, err)
		state.busy[key] = nil
		if state.stopped then
			return
		end
		if admin and err and err.code == "forbidden" then
			clear_admin(state)
			flash(state, "error", "You no longer have admin access.")
			state.cx:render()
			return
		end
		if err or type(link) ~= "table" or type(link.url) ~= "string" then
			fail(state, "Could not open the screen", err)
			state.cx:render()
			return
		end
		local url = link.url
		if string.sub(url, 1, 1) == "/" then
			url = state.api.creds.url .. url
		end
		local what = if admin and d.user then d.user .. "'s " .. d.model else d.model
		if how == "copy" then
			state.cx:copy(url)
			flash(state, "success", "Copied a " .. (if control then "" else "view-only ") .. "link to " .. what .. ". It opens once, within " .. tostring(link.expires_in or 60) .. " s.")
		else
			local float = how == "float"
			ask(state, { kind = "screen", url = url, float = float })
			flash(state, "success", "Opening the screen of " .. what .. (if control then "" else " (view only)") .. (if float then " in a floating window." else " beside this panel."))
		end
		state.cx:render()
	end)
	state.cx:render()
end

local function screenshot(state: any, d: any)
	if not d or d.status ~= "ready" then
		return
	end
	local id = d.lease_id
	if state.busy["shot:" .. id] then
		return
	end
	state.busy["shot:" .. id] = true
	apimod.request(state.api, "GET", "/v1/leases/" .. id .. "/screenshot.jpg", { raw = true, accept = "image/jpeg", timeout_ms = 30000 }, function(bytes, err)
		state.busy["shot:" .. id] = nil
		if state.stopped then
			return
		end
		if err then
			fail(state, "Screenshot failed", err)
		else
			state.shots[id] = { blob = state.cx:blob(bytes, "image/jpeg"), at = fmt.now() }
		end
		state.cx:render()
	end)
	state.cx:render()
end

local function release(state: any, id: string)
	local key = "release:" .. id
	if state.busy[key] then
		return
	end
	state.busy[key] = true
	apimod.tool(state.api, "release_device", { lease_id = id }, 60000, function(_, err)
		state.busy[key] = nil
		if state.stopped then
			return
		end
		if err then
			fail(state, "Could not release the device", err)
		else
			state.shots[id] = nil
			flash(state, "success", "Released " .. id .. ". It no longer counts against your minutes.")
		end
		state.raw.budget = nil
		read(state, "budget", "budget_status")
		state.cx:render()
	end)
	state.cx:render()
end

local function acquire(state: any, platform: string, model: string?)
	if state.booting then
		return
	end
	state.booting = { platform = platform, model = model or platform, at = tern.now() }
	state.view = "devices"
	apimod.tool(state.api, "acquire_device", { platform = platform, model = model }, ACQUIRE_MS, function(lease, err)
		state.booting = nil
		if state.stopped then
			return
		end
		if err then
			fail(state, "Could not boot a device", err)
		elseif type(lease) == "table" then
			state.sel.devices = lease.lease_id
			flash(state, "success", (lease.model or "The device") .. " is ready. Open its screen with o.")
		end
		state.raw.budget = nil
		read(state, "budget", "budget_status")
		state.cx:render()
	end)
	state.cx:render()
end

-- Asks the window for a model picker; it answers with `w_acquire`.
local function new_device(state: any)
	if state.booting then
		flash(state, "warning", "A device is already booting.")
		return
	end
	apimod.tool(state.api, "list_device_models", nil, nil, function(platforms, err)
		if state.stopped then
			return
		end
		if err or type(platforms) ~= "table" then
			fail(state, "Could not list the device models", err)
			state.cx:render()
			return
		end
		local items = {}
		for _, p in platforms do
			local free = (p.capacity or 0) - (p.in_use or 0)
			for _, m in p.models or {} do
				table.insert(items, {
					label = m,
					detail = (if p.platform == "ios" then "iOS simulator" else "Android emulator")
						.. " · " .. free .. " of " .. tostring(p.capacity) .. " free"
						.. (if m == p.default_model then " · default" else ""),
					platform = p.platform,
					model = m,
					current = m == p.default_model and p.platform == "ios",
				})
			end
		end
		if #items == 0 then
			flash(state, "warning", "This Seiji host offers no devices.")
			state.cx:render()
			return
		end
		ask(state, { kind = "pick_model", items = items })
	end)
end

-- Saves `path` to ~/Downloads/<file>. Past what Tern downloads (16 MiB), copies a curl command
-- that does.
local function download(state: any, key: string, path: string, accept: string, file: string, what: string)
	if state.busy[key] then
		return
	end
	local home = tern.getenv("HOME")
	if not home then
		flash(state, "error", "HOME is not set; cannot pick a download folder.")
		return
	end
	state.busy[key] = true
	apimod.request(state.api, "GET", path, { raw = true, accept = accept, timeout_ms = 120000 }, function(bytes, err)
		state.busy[key] = nil
		if state.stopped then
			return
		end
		if err then
			if err.code == "unreachable" and string.find(err.message, "16 MiB", 1, true) then
				local cmd = "curl -fLo " .. file .. " -H \"Authorization: Bearer $(seiji token print)\" "
					.. state.api.creds.url .. path
				state.cx:copy(cmd)
				flash(state, "warning", "The " .. what .. " is over 16 MiB, more than Tern downloads. Copied a curl command that does.")
			else
				fail(state, "Download failed", err)
			end
		else
			local ok, werr = pcall(tern.fs.write, home .. "/Downloads/" .. file, bytes)
			if ok then
				flash(state, "success", "Saved " .. fmt.bytes(#bytes) .. " to ~/Downloads/" .. file)
			else
				flash(state, "error", "Could not save the " .. what .. ": " .. tostring(werr))
			end
		end
		state.cx:render()
	end)
	flash(state, "info", "Downloading the " .. what .. "…")
	state.cx:render()
end

local function download_evidence(state: any, run: any)
	if not run then
		return
	end
	local id = run.run_id
	download(state, "zip:" .. id, "/v1/runs/" .. id .. "/evidence.zip", "application/zip", id .. "-evidence.zip", "evidence of " .. id)
end

-- The last viewer trace (`/v1/admin/viewer/trace`), as JSON lines.
local function download_trace(state: any)
	download(state, "trace", "/v1/admin/viewer/trace", "application/x-ndjson", "seiji-viewer-trace.jsonl", "viewer trace")
end

local function copy_summary(state: any, run: any)
	local rep = run and state.reports[run.run_id]
	if rep and rep.md then
		state.cx:copy(rep.md)
		flash(state, "success", "Copied the Markdown summary of " .. run.run_id .. ". Paste it into the pull request.")
	else
		load_report(state, run)
		flash(state, "info", "The summary is still loading.")
	end
end

local function set_view(state: any, view: string)
	if view ~= "devices" and view ~= "runs" and view ~= "users" then
		return
	end
	if view == "users" and not state.is_admin then
		return
	end
	state.view = view
	state.confirm = nil
	cancel_edit(state)
	if view == "runs" then
		state.read_at.runs = nil
	end
	if view == "users" then
		state.read_at.admin_users = nil
	end
	poll(state)
end

local function move(state: any, step: number)
	local list, field = devices(state), "lease_id"
	if state.view == "runs" then
		list, field = runs(state), "run_id"
	elseif state.view == "users" then
		list, field = admin_users(state), "id"
	end
	if #list == 0 then
		return
	end
	local cur
	if state.view == "users" then
		cur = admin_current(state)
	else
		cur = current(state)
	end
	local at = 1
	for i, it in list do
		if it == cur then
			at = i
		end
	end
	local nxt = list[math.clamp(at + step, 1, #list)]
	state.sel[state.view] = nxt[field]
	if state.view == "runs" then
		load_report(state, nxt)
	end
end

local function on_action(state: any, act: string, value: string?)
	local d = if state.view == "devices" then current(state) else nil
	if act == "sel" and value then
		if state.view == "users" then
			-- Parse numeric id for users.
			state.sel.users = tonumber(value)
		else
			state.sel[state.view] = value
		end
		if state.view == "runs" then
			load_report(state, current(state))
		end
	elseif act == "view" and value then
		set_view(state, value)
	elseif act == "refresh" then
		state.read_at = {}
		state.raw = {}
		state.reports = {}
		state.is_admin = nil
		-- Don't cancel an in-flight save; cancel only an idle edit.
		if not state.saving then
			cancel_edit(state)
		end
		poll(state)
	elseif act == "open" then
		viewer(state, d, true, "beside", false)
	elseif act == "float" then
		viewer(state, d, true, "float", false)
	elseif act == "share" then
		viewer(state, d, false, "copy", false)
	elseif (act == "admin_watch" or act == "admin_control" or act == "admin_link") and value and state.is_admin then
		local ad = find(admin_leases(state), "lease_id", value)
		viewer(state, ad, act == "admin_control", if act == "admin_link" then "copy" else "beside", true)
	elseif act == "shot" then
		screenshot(state, d)
	elseif act == "release" and d then
		state.confirm = {
			title = "Release " .. d.model .. "?",
			line = "Seiji deletes the device and its data; open screens close. Runs and their evidence stay.",
			ok = "Release",
			tone = "error",
			lease = d.lease_id,
		}
	elseif act == "confirm_ok" and state.confirm then
		local lease = state.confirm.lease
		state.confirm = nil
		release(state, lease)
	elseif act == "confirm_cancel" then
		state.confirm = nil
	elseif act == "new" then
		new_device(state)
	elseif act == "copy" then
		copy_summary(state, current(state))
	elseif act == "evidence" then
		download_evidence(state, current(state))
	elseif act == "signin" then
		ask(state, { kind = "signin" })
	elseif act == "retry" then
		state.api.auth_at = -1e9
		poll(state)
	elseif act == "w_acquire" and value then
		local ok, req = pcall(tern.json.decode, value)
		if ok and type(req) == "table" and type(req.platform) == "string" then
			acquire(state, req.platform, req.model)
		end
	elseif act == "w_reauth" then
		state.api.creds, state.api.auth, state.api.auth_at = nil, "checking", -1e9
		state.data, state.raw, state.read_at, state.reports, state.shots = {}, {}, {}, {}, {}
		state.is_admin = nil
		cancel_edit(state)
		poll(state)
	-- Admin Users actions
	elseif act == "edit_user" then
		if not state.saving then
			local u = admin_current(state)
			if u then
				start_edit(state, u)
			end
		end
	elseif act == "cancel_edit" then
		if not state.saving then
			cancel_edit(state)
		end
	elseif act == "save_edit" then
		save_edit(state)
	elseif act == "edit_field" and value then
		-- Block field changes while a save is in flight.
		if not state.saving then
			local ok, fv = pcall(tern.json.decode, value)
			if ok and type(fv) == "table" and state.editing then
				if fv.field == "monthly_unlimited" then
					state.editing.monthly_unlimited = fv.value == true
				elseif fv.field == "monthly_minutes" then
					state.editing.monthly_minutes = tostring(fv.value or "")
				elseif fv.field == "max_concurrent" then
					state.editing.max_concurrent = tostring(fv.value or "")
				end
				state.edit_error = nil
			end
		end
	-- Admin viewer settings: `viewer_pace=<ms>`, `viewer_webrtc=<true|false>`.
	elseif act == "viewer_pace" and value then
		local ms = tonumber(value)
		if ms and ms >= 0 and ms <= 1000 then
			set_viewer(state, { pace_max_ms = math.floor(ms) })
		end
	elseif act == "viewer_webrtc" and value then
		set_viewer(state, { webrtc = value == "true" })
	elseif act == "viewer_trace" and value then
		set_viewer(state, { trace = value == "true" })
	elseif act == "viewer_trace_download" then
		download_trace(state)
	end
end

local function key(state: any, k: Key, cx: any): boolean?
	if k.meta or k.ctrl then
		return false
	end
	local n = k.name
	if state.confirm then
		if n == "enter" then
			on_action(state, "confirm_ok", nil)
			return true
		elseif n == "escape" then
			on_action(state, "confirm_cancel", nil)
			return true
		end
		return false
	end
	if state.api.auth ~= "ok" then
		if n == "enter" and state.api.auth == "logged_out" then
			on_action(state, "signin", nil)
			return true
		elseif n == "r" then
			on_action(state, "retry", nil)
			return true
		end
		return false
	end
	if n == "tab" then
		if state.is_admin then
			local order = { "devices", "runs", "users" }
			local idx = 1
			for i, v in order do
				if v == state.view then
					idx = i
					break
				end
			end
			set_view(state, order[idx % #order + 1])
		else
			set_view(state, if state.view == "devices" then "runs" else "devices")
		end
	elseif n == "up" or n == "k" then
		move(state, -1)
	elseif n == "down" or n == "j" then
		move(state, 1)
	elseif n == "r" then
		on_action(state, "refresh", nil)
	elseif n == "n" and state.view ~= "users" then
		on_action(state, "new", nil)
	elseif state.view == "devices" and (n == "o" or n == "enter") then
		on_action(state, "open", nil)
	elseif state.view == "devices" and n == "f" then
		on_action(state, "float", nil)
	elseif state.view == "devices" and n == "s" then
		on_action(state, "shot", nil)
	elseif state.view == "devices" and n == "l" then
		on_action(state, "share", nil)
	elseif state.view == "devices" and n == "x" then
		on_action(state, "release", nil)
	elseif state.view == "runs" and (n == "c" or n == "enter") then
		on_action(state, "copy", nil)
	elseif state.view == "runs" and n == "d" then
		on_action(state, "evidence", nil)
	elseif state.view == "users" and n == "e" then
		on_action(state, "edit_user", nil)
	elseif state.view == "users" and n == "escape" then
		on_action(state, "cancel_edit", nil)
	elseif state.view == "users" and (n == "enter") then
		if state.editing then
			on_action(state, "save_edit", nil)
		else
			on_action(state, "edit_user", nil)
		end
	else
		return false
	end
	return true
end

M.def = {
	font = "sans",
	init = function(cx: any, args: { string }, saved: any)
		local s = type(saved) == "table" and saved or {}
		local state = {
			cx = cx,
			api = apimod.new(),
			-- "users" is deferred: it needs /v1/me to confirm admin first.
			view = if args[1] == "runs" or s.view == "runs" then "runs" else "devices",
			pending_view = if args[1] == "users" or s.view == "users" then "users" else nil,
			sel = { devices = s.device, runs = s.run, users = s.user_id },
			data = {},
			raw = {},
			errs = {},
			read_at = {},
			inflight = {},
			busy = {},
			reports = {},
			shots = {},
			booting = nil,
			flash = nil,
			confirm = nil,
			updated = nil,
			stopped = false,
			is_admin = nil,
			editing = nil,
			edit_error = nil,
			saving = false,
		}
		local function tick()
			if state.stopped or not running(cx.pane) then
				state.stopped = true
				return
			end
			if state.flash and tern.now() - state.flash.at > FLASH_MS then
				state.flash = nil
				cx:render()
			elseif state.booting then
				cx:render()
			end
			poll(state)
			tern.timer(POLL_MS, tick)
		end
		tick()
		return state
	end,
	view = function(state: any, cx: any)
		current(state)
		state.cols = cx.cols
		return V.render(state, M)
	end,
	title = function(state: any)
		local n = #devices(state)
		if n > 0 then
			return "Seiji · " .. fmt.plural(n, "device", "devices")
		end
		return "Seiji"
	end,
	key = key,
	event = function(state: any, ev: UiEvent, _cx: any)
		if ev.ev == "select" and ev.id == "main.nav" and type(ev.item) == "string" then
			set_view(state, ev.item)
		elseif ev.ev == "action" and type(ev.act) == "string" then
			local value = ev.value
			on_action(state, ev.act, if type(value) == "string" then value elseif value ~= nil then tostring(value) else nil)
		end
	end,
	save = function(state: any)
		return { view = state.view, device = state.sel.devices, run = state.sel.runs, user_id = state.sel.users }
	end,
}

return M
