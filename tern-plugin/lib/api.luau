--!strict
-- Talks to the Seiji server with tern.fetch: MCP `tools/call` (JSON-RPC over Streamable HTTP,
-- stateless) for what agents can do too, REST for the viewer link, screenshots and evidence.
-- The token stays in this table's memory; it never reaches a block's args or `save()`.

local cli = require("./cli")

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
	local ok, doc = pcall(tern.json.decode, body)
	if ok and type(doc) == "table" then
		if type(doc.error) == "table" then
			return { code = "http", message = tostring(doc.error.message or ("HTTP " .. status)) }
		elseif type(doc.error) == "string" then
			return { code = "http", message = doc.error }
		end
	end
	local text = string.match(body or "", "^%s*(.-)%s*$") or ""
	return { code = "http", message = if text ~= "" and #text < 300 then text else "HTTP " .. status }
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

-- Calls MCP tool `name`; `cb(value, err)` with the tool's text decoded as JSON when it is JSON.
function M.tool(c: Client, name: string, args: { [string]: any }?, timeout_ms: number?, cb: (value: any, err: cli.CliError?) -> ())
	c.seq += 1
	local params: { [string]: any } = { name = name }
	if args and next(args) ~= nil then
		params.arguments = args
	end
	M.request(c, "POST", "/mcp", {
		body = { jsonrpc = "2.0", id = c.seq, method = "tools/call", params = params },
		accept = "application/json, text/event-stream",
		headers = { ["MCP-Protocol-Version"] = PROTOCOL },
		timeout_ms = timeout_ms,
	}, function(doc, err)
		if err then
			cb(nil, err)
			return
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
		local ok, value = pcall(tern.json.decode, text)
		cb(if ok then value else text, nil)
	end)
end

return M
