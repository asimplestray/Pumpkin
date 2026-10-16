--- NOTE: mirrored from pumpkin-translator/lua/bukkit.lua; keep in sync.
--- The LuaPluginLoader embeds this file, so Lua plugins get the shim
--- without any extra download.
--- Bukkit compatibility shim for Lua plugins (no JVM).
--
-- The loader injects a global `host` table before running plugin code.
-- Contract (every function the shim needs from the host):
--   host.get_player_by_name(name) -> player handle or nil
--   host.get_all_players() -> list of player handles
--   host.execute_command(command, sender) -> ok boolean
--   host.player_get_name(player) -> string
--   host.player_send_message(player, text)
--   host.player_kick(player, reason)
--   host.player_teleport(player, world, x, y, z, yaw, pitch)
--   host.player_has_permission(player, node) -> boolean
--   host.player_get_world(player) -> world handle
--   host.player_get_location(player) -> {world, x, y, z, yaw, pitch}
--   host.world_get_name(world) -> string
--   host.scheduler_run_delayed(ticks, fn_id) -> task id
--   host.scheduler_run_repeating(delay, period, fn_id) -> task id
--   host.scheduler_cancel(task_id)
--   host.config_path(name) -> writable file path inside the data folder
--   host.log(level, message)
-- Player handles are opaque; the shim wraps them with methods.
-- Event objects are plain tables; Java-style setters map to fields through
-- one metatable, so every event works without per-event code:
--   e:setJoinMessage(m) -> e.join_message = m

local M = {}

-- camelCase method suffix -> snake_case field, e.g. JoinMessage -> join_message
local function field_of(name)
    return name:gsub("(%u)", function(c)
        return "_" .. c:lower()
    end):gsub("^_", "")
end

local event_mt = {
    __index = function(ev, key)
        if type(key) == "string" and key:sub(1, 3) == "set" and #key > 3 then
            local f = field_of(key:sub(4))
            return function(_, v)
                ev[f] = v
            end
        end
        if type(key) == "string" and key:sub(1, 3) == "get" and #key > 3 then
            local f = field_of(key:sub(4))
            return function(_)
                return ev[f]
            end
        end
        return rawget(ev, key)
    end,
}

---Wraps a raw event table so Java-style accessors work on it.
---@param ev table
function M.wrap_event(ev)
    return setmetatable(ev, event_mt)
end

local player_mt
player_mt = {
    __index = function(p, key)
        local raw = rawget(p, "_handle")
        if key == "sendMessage" then
            return function(_, text)
                host.player_send_message(raw, tostring(text))
            end
        elseif key == "getName" then
            return function(_)
                return host.player_get_name(raw)
            end
        elseif key == "kickPlayer" then
            return function(_, reason)
                host.player_kick(raw, reason)
            end
        elseif key == "teleport" then
            return function(_, world, x, y, z, yaw, pitch)
                host.player_teleport(raw, world, x, y, z, yaw or 0, pitch or 0)
            end
        elseif key == "hasPermission" then
            return function(_, node)
                return host.player_has_permission(raw, node)
            end
        elseif key == "performCommand" then
            return function(_, cmd)
                return host.execute_command(cmd, "player:" .. host.player_get_name(raw))
            end
        elseif key == "getWorld" then
            return function(_)
                return host.player_get_world(raw)
            end
        elseif key == "getLocation" then
            return function(_)
                return host.player_get_location(raw)
            end
        end
        return rawget(p, key)
    end,
}

---Wraps a host player handle with Bukkit-style methods.
---@param handle any
function M.wrap_player(handle)
    return setmetatable({ _handle = handle }, player_mt)
end

---Bukkit module surface used by ported plugins.
Bukkit = Bukkit or {}

function Bukkit.getPlayer(name)
    local h = host.get_player_by_name(name)
    if h == nil then
        return nil
    end
    return M.wrap_player(h)
end

function Bukkit.getOnlinePlayers()
    local out = {}
    for _, h in ipairs(host.get_all_players()) do
        out[#out + 1] = M.wrap_player(h)
    end
    return out
end

function Bukkit.dispatchCommand(cmd)
    return host.execute_command(cmd, "console")
end

function Bukkit.getServer()
    return Bukkit
end

-- -- -- config: JSON file in the data folder, mirroring getConfig -- -- --

local json = {}

function json.encode(v)
    local t = type(v)
    if t == "string" then
        return string.format("%q", v)
    elseif t == "number" or t == "boolean" then
        return tostring(v)
    elseif t == "table" then
        local is_list, n = true, 0
        for k, _ in pairs(v) do
            n = n + 1
            if type(k) ~= "number" or k ~= n then
                is_list = false
                break
            end
        end
        local parts = {}
        if is_list then
            for i = 1, n do
                parts[#parts + 1] = json.encode(v[i])
            end
            return "[" .. table.concat(parts, ",") .. "]"
        end
        for k, val in pairs(v) do
            parts[#parts + 1] = string.format("%q", tostring(k)) .. ":" .. json.encode(val)
        end
        return "{" .. table.concat(parts, ",") .. "}"
    end
    return "null"
end

-- Minimal JSON reader: flat/nested objects of string/number/boolean,
-- lists, and null. Anything else returns nil + error.
function json.decode(s)
    local pos = 1
    local function err()
        return nil, "parse error at " .. tostring(pos)
    end
    local parse_value, parse_object, parse_array, parse_string, parse_literal
    local function skip()
        while pos <= #s and s:sub(pos, pos):match("%s") do
            pos = pos + 1
        end
    end
    function parse_string()
        assert(s:sub(pos, pos) == '"')
        pos = pos + 1
        local buf = {}
        while pos <= #s do
            local c = s:sub(pos, pos)
            if c == '"' then
                pos = pos + 1
                return table.concat(buf)
            elseif c == "\\" then
                local e = s:sub(pos + 1, pos + 1)
                local map = { n = "\n", t = "\t", r = "\r", ['"'] = '"', ["\\"] = "\\", ["/"] = "/" }
                if map[e] then
                    buf[#buf + 1] = map[e]
                    pos = pos + 2
                else
                    return err()
                end
            else
                buf[#buf + 1] = c
                pos = pos + 1
            end
        end
        return err()
    end
    function parse_literal()
        local rest = s:sub(pos)
        if rest:sub(1, 4) == "true" then
            pos = pos + 4
            return true
        elseif rest:sub(1, 5) == "false" then
            pos = pos + 5
            return false
        elseif rest:sub(1, 4) == "null" then
            pos = pos + 4
            return nil
        end
        local num = rest:match("^-?%d+%.?%d*")
        if num then
            pos = pos + #num
            return tonumber(num)
        end
        return err()
    end
    function parse_array()
        assert(s:sub(pos, pos) == "[")
        pos = pos + 1
        local out = {}
        skip()
        if s:sub(pos, pos) == "]" then
            pos = pos + 1
            return out
        end
        while true do
            local v, e = parse_value()
            if e then
                return nil, e
            end
            out[#out + 1] = v
            skip()
            local c = s:sub(pos, pos)
            if c == "]" then
                pos = pos + 1
                return out
            elseif c ~= "," then
                return err()
            end
            pos = pos + 1
        end
    end
    function parse_object()
        assert(s:sub(pos, pos) == "{")
        pos = pos + 1
        local out = {}
        skip()
        if s:sub(pos, pos) == "}" then
            pos = pos + 1
            return out
        end
        while true do
            skip()
            if s:sub(pos, pos) ~= '"' then
                return err()
            end
            local k = parse_string()
            skip()
            if s:sub(pos, pos) ~= ":" then
                return err()
            end
            pos = pos + 1
            local v, e = parse_value()
            if e then
                return nil, e
            end
            out[k] = v
            skip()
            local c = s:sub(pos, pos)
            if c == "}" then
                pos = pos + 1
                return out
            elseif c ~= "," then
                return err()
            end
            pos = pos + 1
        end
    end
    function parse_value()
        skip()
        local c = s:sub(pos, pos)
        if c == "{" then
            return parse_object()
        elseif c == "[" then
            return parse_array()
        elseif c == '"' then
            return parse_string()
        end
        return parse_literal()
    end
    local v, e = parse_value()
    if e then
        return nil, e
    end
    skip()
    if pos <= #s then
        return nil, "trailing data"
    end
    return v
end

M.json = json

---Loads name.json from the data folder, writing `default` first if missing.
---@param name string
---@param default table
function M.load_or_create_config(name, default)
    local path = host.config_path(name)
    local f = io.open(path, "r")
    if f then
        local text = f:read("*a")
        f:close()
        local v, e = json.decode(text)
        if v ~= nil then
            return v
        end
        error("config parse error in " .. name .. ": " .. tostring(e))
    end
    local out = io.open(path, "w")
    if not out then
        error("cannot write config " .. name)
    end
    out:write(json.encode(default))
    out:close()
    return default
end

-- -- -- scheduler (tick-based; async tasks run on-tick with a warning) -- --

M.scheduler = {}
local pending_async_warned = false

function M.scheduler.runTaskLater(ticks, fn)
    return host.scheduler_run_delayed(ticks, fn)
end

function M.scheduler.runTaskTimer(delay, period, fn)
    return host.scheduler_run_repeating(delay, period, fn)
end

function M.scheduler.runTaskAsynchronously(fn)
    if not pending_async_warned then
        pending_async_warned = true
        host.log("warn", "runTaskAsynchronously runs on the tick thread in this port")
    end
    return host.scheduler_run_delayed(0, fn)
end

function M.scheduler.cancelTask(id)
    host.scheduler_cancel(id)
end

-- -- -- transient metadata (Metadatable shim; host PDC arrives later) -- --

local metadata_store = setmetatable({}, { __mode = "k" })

local function meta_of(obj)
    local m = metadata_store[obj]
    if not m then
        m = {}
        metadata_store[obj] = m
    end
    return m
end

function M.setMetadata(obj, key, value)
    meta_of(obj)[key] = value
end

function M.getMetadata(obj, key)
    local m = metadata_store[obj]
    if m then
        return m[key]
    end
    return nil
end

function M.hasMetadata(obj, key)
    local m = metadata_store[obj]
    return m ~= nil and m[key] ~= nil
end

function M.removeMetadata(obj, key)
    local m = metadata_store[obj]
    if m then
        m[key] = nil
    end
end

-- -- -- services (ServicesManager/Vault stand-in) -- --

M.services = {}

function M.services.register(name, provider)
    M.services[name] = provider
end

function M.services.get(name)
    return M.services[name]
end

return M
