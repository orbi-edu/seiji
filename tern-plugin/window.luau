--!strict
-- Window half: the palette commands, the status-line segment, and the window-only UI the block
-- asks for through `seiji://ui?req=…` links (the model picker, the sign-in tab, the screen).
-- It also claims Seiji viewer links (`…/view/open/<ticket>`) clicked anywhere, such as the one
-- an agent's `share_screen` prints, and opens them in Tern's browser beside the pane.

local cli = require("./lib/cli")
local fmt = require("./lib/fmt")
local sse = require("./lib/sse")

local APP = "seiji.app"
local POLL_MS = 15000

local function find_block(cx: any, kind: string): any
	for _, p in cx.session:panes() do
		if p.block == kind then
			return p
		end
	end
	return nil
end

local function send(cx: any, pane: number, act: string, payload: { [string]: any })
	cx.session:event(pane, { ev = "action", id = "main", act = act, value = tern.json.encode(payload) })
end

local function open_app(cx: any, view: string?)
	local p = find_block(cx, APP)
	if p then
		if p.parked then
			cx.layout:unpark(p.pane)
		else
			cx.layout:focus(p.pane)
		end
		if view then
			cx.session:event(p.pane, { ev = "action", id = "main", act = "view", value = view })
		end
		return
	end
	cx:new_block(APP, if view then { view } else nil, "tab")
end

-- The screen: Tern's browser beside the pane, or a picture in picture floating over it.
local function open_screen(cx: any, url: string, pane: number, float: boolean)
	if float then
		cx.browser:call({ op = "open", owner = pane, url = url }):next(function(res, err, cx2)
			local why = err or (type(res) == "table" and res.error and res.error.message)
			if why and cx2 then
				cx2:toast("error", "Could not float the screen", tostring(why))
			end
		end)
		return
	end
	cx:open(url, "beside")
end

-- Opens a shell tab and types `command` into it; the shell reads it once it is up.
local function new_tab(cx: any, command: string, name: string)
	local pane = cx.layout:new_tab(nil)
	if not pane then
		cx:toast("error", "Could not open a new tab")
		return
	end
	cx:run(pane, command .. "\r")
	local tab = cx.session:tab_of(pane)
	if tab then
		cx.layout:name_tab(tab, name)
	end
end

local function signin(cx: any)
	cx:prompt({ placeholder = "Seiji server URL (https://seiji.example.com)", icon = "log-in", action = "continue" })
		:next(function(url, _err, cx2)
			if not url or not cx2 then
				return
			end
			url = string.match(url, "^%s*(.-)%s*$") or ""
			if not string.match(url, "^https?://[%w%.%-]+[:%d]*/?$") then
				cx2:toast("error", "Enter the server's address, like https://seiji.example.com")
				return
			end
			-- `seiji login` asks for the token without echo and keeps it in the keychain.
			new_tab(cx2, cli.bin() .. " login --url " .. fmt.shq(url), "Seiji sign-in")
		end)
end

local function pick_model(cx: any, req: any, pane: number)
	local items = {}
	for _, it in (type(req.items) == "table" and req.items) or {} do
		if type(it) == "table" and type(it.label) == "string" then
			local row = { label = it.label, detail = it.detail, icon = "monitor", platform = it.platform, model = it.model }
			if it.current then
				table.insert(items, 1, row)
			else
				table.insert(items, row)
			end
		end
	end
	if #items == 0 then
		return
	end
	cx:pick({ items = items, placeholder = "Boot which device?", icon = "monitor", action = "boot" }):next(function(item, _err, cx2)
		if item and cx2 then
			send(cx2, pane, "w_acquire", { platform = item.platform, model = item.model })
		end
	end)
end

local function handle(cx: any, req: any, pane: number)
	if req.kind == "screen" and type(req.url) == "string" and fmt.is_viewer_url(req.url) then
		open_screen(cx, req.url, pane, req.float == true)
	elseif req.kind == "pick_model" then
		pick_model(cx, req, pane)
	elseif req.kind == "signin" then
		signin(cx)
	end
end

tern.route.link(function(link, cx)
	-- A viewer link from any pane: an agent's `share_screen`, or a link pasted in a terminal.
	if fmt.is_viewer_url(link.url) then
		local url, pane, float = link.url, link.pane, link.mods.alt
		tern.timer(0, function(cx2)
			if cx2 then
				open_screen(cx2, url, pane, float)
			end
		end)
		return { handled = true }
	end
	if string.sub(link.url, 1, #fmt.UI_PREFIX) ~= fmt.UI_PREFIX then
		return nil
	end
	-- Only our own block may ask, and the request must name the pane it came from.
	local req = fmt.parse_ui_link(link.url)
	local info = cx.session:pane(link.pane)
	if not req or req.pane ~= link.pane or not info or info.block ~= APP then
		tern.log.warn("seiji: ignored link from pane", link.pane)
		return { handled = true }
	end
	-- A route hook has a 50 ms budget: answer now, act on the next tick.
	local pane = link.pane
	tern.timer(0, function(cx2)
		if cx2 then
			handle(cx2, req, pane)
		end
	end)
	return { handled = true }
end)

-- ── status line ────────────────────────────────────────────────────────────

local status = { creds = nil :: any, segment = nil :: any, key = "", authing = false }

local function publish(seg: any)
	local k = if seg then seg.text .. "|" .. (seg.tone or "") else ""
	if k ~= status.key then
		status.key = k
		status.segment = seg
		tern.chrome.refresh()
	end
end

local function status_poll()
	local creds = status.creds
	if not creds then
		if status.authing then
			return
		end
		status.authing = true
		local ok = cli.credential(function(c, e)
			status.authing = false
			status.creds = c
			if c then
				status_poll()
			elseif e and e.code == "not_logged_in" then
				publish({ text = "Seiji · sign in", icon = "log-in", tone = "muted", command = "plugin.seiji.devices" })
			else
				publish(nil)
			end
		end)
		if not ok then
			status.authing = false
		end
		return
	end
	tern.fetch(creds.url .. "/mcp", {
		method = "POST",
		headers = {
			Authorization = "Bearer " .. creds.token,
			["User-Agent"] = "seiji-tern/1",
			["Content-Type"] = "application/json",
			Accept = "application/json, text/event-stream",
			["MCP-Protocol-Version"] = "2025-11-25",
		},
		body = tern.json.encode({ jsonrpc = "2.0", id = 1, method = "tools/call", params = { name = "budget_status" } }),
		timeout_ms = 5000,
	}, function(r)
		if r.status == 401 then
			status.creds = nil
			publish(nil)
			return
		end
		if r.error or r.status ~= 200 then
			publish({ text = "Seiji · offline", icon = "monitor", tone = "error", command = "plugin.seiji.devices" })
			return
		end
		local ok, b = pcall(function()
			-- Try JSON first (application/json), then SSE (text/event-stream).
			local json_ok, doc = pcall(tern.json.decode, r.body)
			if not json_ok then
				doc = sse.parse(r.body, 1)
			end
			return tern.json.decode(doc.result.content[1].text)
		end)
		if not ok or type(b) ~= "table" then
			return
		end
		local n = #(b.active_leases or {})
		local left = b.remaining_minutes or 0
		local quota = b.monthly_minutes or 0
		local tone = if quota > 0 and left / quota < 0.1 then "error" elseif quota > 0 and left / quota < 0.25 then "warning" else "muted"
		local parts = { "Seiji" }
		if n > 0 then
			table.insert(parts, fmt.plural(n, "device", "devices"))
		end
		table.insert(parts, fmt.num(left) .. " min left")
		publish({ text = table.concat(parts, " · "), icon = "monitor", tone = tone, command = "plugin.seiji.devices" })
	end)
end

local function status_loop()
	status_poll()
	tern.timer(POLL_MS, status_loop)
end

tern.chrome.status(function(_pane)
	if status.segment then
		return { status.segment }
	end
	return nil
end)

status_loop()

-- ── palette ────────────────────────────────────────────────────────────────

tern.command({
	id = "devices",
	title = "Seiji: Devices",
	icon = "monitor",
	keys = { "ctrl+alt+d" },
	run = function(cx)
		open_app(cx, "devices")
	end,
})

tern.command({
	id = "runs",
	title = "Seiji: Runs",
	icon = "reel",
	run = function(cx)
		open_app(cx, "runs")
	end,
})

tern.command({
	id = "signin",
	title = "Seiji: Sign in",
	icon = "log-in",
	run = function(cx)
		signin(cx)
	end,
})

tern.command({
	id = "signout",
	title = "Seiji: Sign out",
	icon = "log-out",
	run = function(cx)
		cli.run({ "logout" }, nil, function(_, e, cx2)
			if not cx2 then
				return
			end
			if e then
				cx2:toast("error", "Sign-out failed", cli.describe(e))
				return
			end
			status.creds = nil
			publish(nil)
			for _, p in cx2.session:panes() do
				if p.block == APP then
					send(cx2, p.pane, "w_reauth", {})
				end
			end
			cx2:toast("info", "Signed out of Seiji")
		end)
	end,
})
