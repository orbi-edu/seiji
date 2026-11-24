--!strict
-- Talks to the Seiji server with tern.fetch: MCP `tools/call` (JSON-RPC over Streamable HTTP,
-- stateless, JSON or SSE) for what agents can do too, REST for the viewer link, screenshots
-- and evidence. The token stays in this table's memory; it never reaches a block's args or
-- `save()`.

local cli = require("./cli")
local sse = require("./sse")

local M = {}

export type Client = {
	creds: any,
	auth: string, -- "checking" | "ok" | "logged_out" | "cli_missing" | "error"
	auth_error: cli.CliError?,
	auth_at: number,
	authing: boolean,
	seq: number,
}

local PROTOCOL = "2025-11-25"

function M.new(): Client
	return { creds = nil, auth = "checking", auth_error = nil, auth_at = -1e9, authing = false, seq = 0 }
end

-- Asks the CLI for the credential; `done()` runs once `c.auth` settles.
function M.authenticate(c: Client, done: () -> ())
	if c.authing then
		return
	end
	c.authing = true
	c.auth_at = tern.now()
	local ok, err = cli.credential(function(creds, e)
		c.authing = false
		if e then
			c.creds = nil
			c.auth = if e.code == "not_logged_in" then "logged_out" elseif e.code == "cli_missing" then "cli_missing" else "error"
			c.auth_error = e
		else
			c.creds = creds
			c.auth = "ok"
			c.auth_error = nil
		end
		done()
	end)
	if not ok then
		c.authing = false
		c.auth = "cli_missing"
		c.auth_error = err
		done()
	end
end

local function http_error(status: number, body: string): cli.CliError
	local code = if status == 403 then "forbidden" elseif status == 404 then "not_found" else "http"
	local ok, doc = pcall(tern.json.decode, body)
	if ok and type(doc) == "table" then
		if type(doc.error) == "table" then
			return { code = code, message = tostring(doc.error.message or ("HTTP " .. status)), status = status }
		elseif type(doc.error) == "string" then
			return { code = code, message = doc.error, status = status }
		end
	end
	local text = string.match(body or "", "^%s*(.-)%s*$") or ""
	return { code = code, message = if text ~= "" and #text < 300 then text else "HTTP " .. status, status = status }
end

-- `opts`: `body` (a table, sent as JSON), `raw` (hand back the body bytes), `timeout_ms`.
function M.request(c: Client, method: string, path: string, opts: { [string]: any }?, cb: (data: any, err: cli.CliError?) -> ())
	local creds = c.creds
	if not creds then
		cb(nil, { code = "not_logged_in", message = "" })
		return
	end
	local o = opts or {}
	local headers: { [string]: string } = {
		Authorization = "Bearer " .. creds.token,
		Accept = o.accept or "application/json",
		["User-Agent"] = "seiji-tern/1",
	}
	local body = nil
	if o.body ~= nil then
		headers["Content-Type"] = "application/json"
		body = tern.json.encode(o.body)
	end
	for k, v in o.headers or {} do
		headers[k] = v
	end
	tern.fetch(creds.url .. path, { method = method, headers = headers, body = body, timeout_ms = o.timeout_ms or 15000 }, function(r)
		if r.error then
			cb(nil, { code = if r.timed_out then "timeout" else "unreachable", message = tostring(r.error) })
			return
		end
		if r.status == 401 then
			c.creds = nil
			c.auth = "checking"
			c.auth_at = -1e9
			cb(nil, { code = "unauthorized", message = "" })
			return
		end
		if r.status < 200 or r.status >= 300 then
			cb(nil, http_error(r.status, r.body))
			return
		end
		if o.raw then
			cb(r.body, nil)
			return
		end
		local ok, doc = pcall(tern.json.decode, r.body)
		if not ok then
			cb(nil, { code = "bad_response", message = "" })
			return
		end
		cb(doc, nil)
	end)
end

-- `GET /v1/me`: the signed-in user's identity and admin flag.
function M.me(c: Client, cb: (data: any, err: cli.CliError?) -> ())
	M.request(c, "GET", "/v1/me", nil, cb)
end

-- `GET /v1/admin/users`: the full user list, host capacity and in-use counts. Admin only.
function M.admin_users(c: Client, cb: (data: any, err: cli.CliError?) -> ())
	M.request(c, "GET", "/v1/admin/users", nil, cb)
end

-- `PUT /v1/admin/users/{id}/limits`: update a user's editable limits. Admin only.
function M.admin_update_user(c: Client, user_id: number, body: { [string]: any }, cb: (data: any, err: cli.CliError?) -> ())
	M.request(c, "PUT", "/v1/admin/users/" .. tostring(user_id) .. "/limits", { body = body }, cb)
end

-- `GET /v1/admin/leases`: every active lease with its owner (`user_id`, `user`). Admin only.
function M.admin_leases(c: Client, cb: (data: any, err: cli.CliError?) -> ())
	M.request(c, "GET", "/v1/admin/leases", nil, cb)
end

-- Calls MCP tool `name`; `cb(value, err)` with the tool's text decoded as JSON when it is JSON.
-- Handles both application/json and text/event-stream responses.
function M.tool(c: Client, name: string, args: { [string]: any }?, timeout_ms: number?, cb: (value: any, err: cli.CliError?) -> ())
	c.seq += 1
	local id = c.seq
	local params: { [string]: any } = { name = name }
	if args and next(args) ~= nil then
		params.arguments = args
	end
	M.request(c, "POST", "/mcp", {
		body = { jsonrpc = "2.0", id = id, method = "tools/call", params = params },
		accept = "application/json, text/event-stream",
		headers = { ["MCP-Protocol-Version"] = PROTOCOL },
		timeout_ms = timeout_ms,
		raw = true,
	}, function(body, err)
		if err then
			cb(nil, err)
			return
		end
		if type(body) ~= "string" then
			cb(nil, { code = "bad_response", message = "" })
			return
		end
		-- Try JSON first (application/json), then SSE (text/event-stream).
		local ok, doc = pcall(tern.json.decode, body :: string)
		if not ok then
			local sse_doc, sse_err = sse.parse(body :: string, id)
			if sse_err then
				cb(nil, { code = "bad_response", message = sse_err })
				return
			end
			doc = sse_doc
		end
		if type(doc) ~= "table" then
			cb(nil, { code = "bad_response", message = "" })
			return
		end
		if type(doc.error) == "table" then
			cb(nil, { code = "tool", message = tostring(doc.error.message or "MCP error") })
			return
		end
		local result = doc.result or {}
		local first = type(result.content) == "table" and result.content[1] or nil
		local text = if type(first) == "table" and type(first.text) == "string" then first.text else ""
		if result.isError then
			cb(nil, { code = "tool", message = text })
			return
		end
		local parsed_ok, value = pcall(tern.json.decode, text)
		cb(if parsed_ok then value else text, nil)
	end)
end

return M
