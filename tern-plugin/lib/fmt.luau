--!strict
-- Formatting shared by both halves: durations, relative times and the `seiji://ui?req=` links a
-- block opens to ask its window for window-only UI.

local M = {}

function M.plural(n: number, one: string, many: string): string
	return tostring(n) .. " " .. (if n == 1 then one else many)
end

-- Seconds since the epoch, now.
function M.now(): number
	return os.time()
end

-- "now", "12 s ago", "3 min ago", "2 h ago", "4 d ago" from epoch seconds.
function M.ago(t: any): string
	if type(t) ~= "number" then
		return "—"
	end
	local s = M.now() - t
	if s < 5 then
		return "now"
	elseif s < 60 then
		return s .. " s ago"
	elseif s < 3600 then
		return s // 60 .. " min ago"
	elseif s < 86400 then
		return s // 3600 .. " h ago"
	end
	return s // 86400 .. " d ago"
end

-- "in 12 min", "in 1 h 5 min" until epoch seconds `t`.
function M.until_(t: any): string
	if type(t) ~= "number" then
		return "—"
	end
	local s = t - M.now()
	if s <= 0 then
		return "now"
	elseif s < 3600 then
		return "in " .. math.max(1, s // 60) .. " min"
	end
	local m = s // 60
	return "in " .. m // 60 .. " h " .. m % 60 .. " min"
end

-- 12.5 → "12.5", 12 → "12".
function M.num(v: any): string
	if type(v) ~= "number" then
		return "—"
	end
	if v == math.floor(v) then
		return tostring(math.floor(v))
	end
	return string.format("%.1f", v)
end

function M.money(v: any): string
	if type(v) ~= "number" then
		return "—"
	end
	return "$" .. string.format("%.2f", v)
end

function M.bytes(v: any): string
	if type(v) ~= "number" then
		return "—"
	end
	if v >= 1048576 then
		return string.format("%.1f MB", v / 1048576)
	elseif v >= 1024 then
		return string.format("%.0f KB", v / 1024)
	end
	return v .. " B"
end

function M.urlencode(s: string): string
	return (string.gsub(s, "[^%w%-%._~]", function(c)
		return string.format("%%%02X", string.byte(c))
	end))
end

function M.urldecode(s: string): string
	return (string.gsub(s, "%%(%x%x)", function(h)
		return string.char(tonumber(h, 16) :: number)
	end))
end

M.UI_PREFIX = "seiji://ui?req="

function M.ui_link(req: { [string]: any }): string
	return M.UI_PREFIX .. M.urlencode(tern.json.encode(req))
end

function M.parse_ui_link(url: string): { [string]: any }?
	if string.sub(url, 1, #M.UI_PREFIX) ~= M.UI_PREFIX then
		return nil
	end
	local ok, req = pcall(tern.json.decode, M.urldecode(string.sub(url, #M.UI_PREFIX + 1)))
	if ok and type(req) == "table" then
		return req
	end
	return nil
end

-- A viewer link: `http(s)://host[:port]/view/open/<ticket>`.
function M.is_viewer_url(url: string): boolean
	return string.match(url, "^https?://[^/%s]+/view/open/%x+$") ~= nil
end

-- POSIX single-quoting for command lines typed into a new tab.
function M.shq(s: string): string
	return "'" .. string.gsub(s, "'", "'\\''") .. "'"
end

return M
