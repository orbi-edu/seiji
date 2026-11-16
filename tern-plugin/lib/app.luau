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
end

-- ── actions ────────────────────────────────────────────────────────────────

local function viewer(state: any, d: any, control: boolean, float: boolean)
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
	apimod.request(state.api, "POST", "/v1/leases/" .. d.lease_id .. "/viewer", { body = { control = control } }, function(link, err)
		state.busy[key] = nil
		if state.stopped then
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
		if control then
			ask(state, { kind = "screen", url = url, float = float })
			flash(state, "success", "Opening the screen of " .. d.model .. (if float then " in a floating window." else " beside this panel."))
		else
			state.cx:copy(url)
			flash(state, "success", "Copied a view-only link to " .. d.model .. ". It opens once, within " .. tostring(link.expires_in or 60) .. " s.")
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

local function download_evidence(state: any, run: any)
	if not run then
		return
	end
	local id = run.run_id
	local key = "zip:" .. id
	if state.busy[key] then
		return
	end
	local home = tern.getenv("HOME")
	if not home then
		flash(state, "error", "HOME is not set; cannot pick a download folder.")
		return
	end
	state.busy[key] = true
	apimod.request(state.api, "GET", "/v1/runs/" .. id .. "/evidence.zip", { raw = true, accept = "application/zip", timeout_ms = 120000 }, function(bytes, err)
		state.busy[key] = nil
		if state.stopped then
			return
		end
		if err then
			if err.code == "unreachable" and string.find(err.message, "16 MiB", 1, true) then
				local cmd = "curl -fLo " .. id .. "-evidence.zip -H \"Authorization: Bearer $(seiji token print)\" "
					.. state.api.creds.url .. "/v1/runs/" .. id .. "/evidence.zip"
				state.cx:copy(cmd)
				flash(state, "warning", "The evidence is over 16 MiB, more than Tern downloads. Copied a curl command that does.")
			else
				fail(state, "Download failed", err)
			end
		else
			local path = home .. "/Downloads/" .. id .. "-evidence.zip"
			local ok, werr = pcall(tern.fs.write, path, bytes)
			if ok then
				flash(state, "success", "Saved " .. fmt.bytes(#bytes) .. " to ~/Downloads/" .. id .. "-evidence.zip")
			else
				flash(state, "error", "Could not save the evidence: " .. tostring(werr))
			end
		end
		state.cx:render()
	end)
	flash(state, "info", "Downloading the evidence of " .. id .. "…")
	state.cx:render()
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
	if view ~= "devices" and view ~= "runs" then
		return
	end
	state.view = view
	state.confirm = nil
	if view == "runs" then
		state.read_at.runs = nil
	end
	poll(state)
end

local function move(state: any, step: number)
	local list, field = devices(state), "lease_id"
	if state.view == "runs" then
		list, field = runs(state), "run_id"
	end
	if #list == 0 then
		return
	end
	local cur = current(state)
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
		state.sel[state.view] = value
		if state.view == "runs" then
			load_report(state, current(state))
		end
	elseif act == "view" and value then
		set_view(state, value)
	elseif act == "refresh" then
		state.read_at = {}
		state.raw = {}
		state.reports = {}
		poll(state)
	elseif act == "open" then
		viewer(state, d, true, false)
	elseif act == "float" then
		viewer(state, d, true, true)
	elseif act == "share" then
		viewer(state, d, false, false)
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
		poll(state)
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
		set_view(state, if state.view == "devices" then "runs" else "devices")
	elseif n == "up" or n == "k" then
		move(state, -1)
	elseif n == "down" or n == "j" then
		move(state, 1)
	elseif n == "r" then
		on_action(state, "refresh", nil)
	elseif n == "n" then
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
			view = if args[1] == "runs" or s.view == "runs" then "runs" else "devices",
			sel = { devices = s.device, runs = s.run },
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
		return { view = state.view, device = state.sel.devices, run = state.sel.runs }
	end,
}

return M
