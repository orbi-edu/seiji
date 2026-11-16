--!strict
-- The `seiji` CLI owns the credential (keychain): both halves read it with
-- `seiji token print --json` and keep it in memory only. `run` calls `cb(data, err, cx)` with the
-- decoded JSON stdout on exit 0, or a `{code, message}` error; it returns `false, err` when the
-- CLI can't be spawned.

local M = {}

export type CliError = { code: string, message: string }

-- `SEIJI_BIN` names the CLI when the daemon's PATH lacks it (Tern started from the Dock).
function M.bin(): string
	local b = tern.getenv("SEIJI_BIN")
	if b and b ~= "" then
		return b
	end
	return "seiji"
end

M.MISSING = {
	code = "cli_missing",
	message = "The seiji CLI is not on Tern's PATH. Install seiji or set SEIJI_BIN to its path.",
}

local function trim(s: string): string
	return string.match(s, "^%s*(.-)%s*$") or ""
end

function M.run(args: { string }, opts: { stdin: string?, timeout_ms: number? }?, cb: (data: any, err: CliError?, cx: any) -> ()): (boolean, CliError?)
	local argv = { M.bin() }
	for _, a in args do
		table.insert(argv, a)
	end
	local o = opts or {}
	local ok, spawn_err = pcall(tern.process.run, argv, {
		stdin = o.stdin,
		timeout_ms = o.timeout_ms or 10000,
		env = { NO_COLOR = "1" },
	}, function(r: any, cx: any)
		local body = trim(r.stdout)
		local decoded, data = false, nil
		if body ~= "" then
			decoded, data = pcall(tern.json.decode, body)
		end
		if not decoded then
			data = nil
		end
		if r.status == 0 then
			cb(data or {}, nil, cx)
			return
		end
		local err: CliError
		if r.timed_out then
			err = { code = "timeout", message = "The seiji CLI did not respond in time." }
		elseif type(data) == "table" and type(data.error) == "table" then
			err = { code = tostring(data.error.code or "error"), message = tostring(data.error.message or "") }
		elseif r.status == 127 then
			err = M.MISSING
		else
			local text = trim(if r.stderr ~= "" then r.stderr else r.stdout)
			err = { code = "cli_failed", message = if text ~= "" then text else "The CLI exited with status " .. r.status }
		end
		cb(nil, err, cx)
	end)
	if not ok then
		tern.log.warn("seiji CLI spawn failed", spawn_err)
		return false, M.MISSING
	end
	return true, nil
end

-- Reads the credential; `cb(creds, err, cx)` with `creds = {url, token, user?}`.
function M.credential(cb: (creds: any, err: CliError?, cx: any) -> ()): (boolean, CliError?)
	return M.run({ "token", "print", "--json" }, nil, function(data, e, cx)
		if e then
			cb(nil, e, cx)
		elseif type(data) == "table" and type(data.url) == "string" and type(data.token) == "string" then
			cb({ url = (string.gsub(data.url, "/+$", "")), token = data.token, user = data.user }, nil, cx)
		else
			cb(nil, { code = "cli_failed", message = "seiji token print --json returned an unexpected format." }, cx)
		end
	end)
end

local KNOWN = {
	not_logged_in = "You are not signed in to Seiji yet.",
	cli_missing = M.MISSING.message,
	timeout = "Seiji did not respond in time.",
	unreachable = "Could not reach Seiji.",
	bad_response = "Seiji sent a response Tern could not read.",
	unauthorized = "The token was rejected (revoked, or the user is disabled). Sign in again.",
}

-- One line for a person. Tool and HTTP errors carry Seiji's own English message.
function M.describe(err: CliError?): string
	if not err then
		return ""
	end
	if err.code == "cli_failed" and err.message ~= "" then
		return "The seiji CLI failed: " .. err.message
	end
	if (err.code == "tool" or err.code == "http" or err.code == "unreachable") and err.message ~= "" then
		return (if err.code == "unreachable" then "Could not reach Seiji: " else "") .. err.message
	end
	return KNOWN[err.code] or (if err.message ~= "" then err.message else "Seiji failed (" .. tostring(err.code) .. ").")
end

return M
