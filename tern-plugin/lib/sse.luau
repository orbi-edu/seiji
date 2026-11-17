--!strict
-- SSE event stream parser for MCP JSON-RPC responses.  Used by the block
-- (api.luau) and the window (window.luau) status line, which both talk to
-- the /mcp endpoint and must handle text/event-stream bodies.

local M = {}

--- Extracts the JSON-RPC response with the given `id` from an SSE body.
--- Returns `(doc, nil)` on match, `(nil, message)` on malformed/truncated,
--- `(nil, nil)` when no matching response is found.
--- Ignores comments, priming events, notifications, server requests and
--- unrelated ids.
function M.parse(body: string, id: number): (any?, string?)
	-- Normalize \r\n and lone \r to \n (the SSE spec allows all three).
	local text = string.gsub(string.gsub(body, "\r\n", "\n"), "\r", "\n")
	local data_parts: { string } = {}
	local has_data = false
	local pos = 1
	local len = #text
	while pos <= len do
		local nl = string.find(text, "\n", pos, true)
		local line: string
		if nl then
			line = string.sub(text, pos, nl - 1)
			pos = nl + 1
		else
			line = string.sub(text, pos)
			pos = len + 1
		end
		if line == "" then
			if has_data then
				local payload = table.concat(data_parts, "\n")
				if string.match(payload, "^%s*$") then
					-- Priming event (empty data); skip.
				else
					local ok, doc = pcall(tern.json.decode, payload)
					if not ok then
						return nil, "malformed JSON in SSE event"
					end
					-- Accept only JSON-RPC responses, not server requests.
					if type(doc) == "table" and doc.id == id and doc.method == nil
						and (doc.result ~= nil or doc.error ~= nil) then
						return doc, nil
					end
				end
				data_parts = {}
				has_data = false
			end
		elseif string.sub(line, 1, 1) == ":" then
			-- SSE comment (keep-alive heartbeat); skip.
		else
			local value = string.match(line, "^data: ?(.*)")
			if value then
				table.insert(data_parts, value)
				has_data = true
			end
			-- event:, id:, retry: fields are not needed; skip.
		end
	end
	if has_data then
		return nil, "SSE stream truncated"
	end
	return nil, nil
end

return M
